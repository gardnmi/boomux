use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::federation::FederationHandshake;
use crate::protocol::{self, AttachFrame, Envelope, Request, Response};

const MAX_SSH_TARGET_BYTES: usize = 1024;
const MAX_REMOTE_EXECUTABLE_BYTES: usize = 4096;
const MAX_CONTROL_PATH_BYTES: usize = 100;
const MAX_PROBE_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_DISCOVERED_EXECUTABLES: usize = 32;
const MAX_PROBE_STDERR_BYTES: usize = 16 * 1024;
const MAX_PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PLATFORM_PROBE_PREFIX: &[u8] = b"boomux-platform-v1";
const EXECUTABLE_PROBE_PREFIX: &[u8] = b"boomux-executables-v1";
const INSTALL_DESTINATION_PROBE_PREFIX: &[u8] = b"boomux-install-destination-v1";
const INSTALL_TRANSACTION_PREFIX: &[u8] = b"boomux-install-transaction-v1";
const INSTALL_ACTIVATION_PREFIX: &[u8] = b"boomux-install-activation-v1";
const INSTALL_COMMIT_PREFIX: &[u8] = b"boomux-install-commit-v1";
const INSTALL_STAGE_PREFIX: &str = "boomux-install-stage-v1";
const RUNTIME_STAGE_PREFIX: &str = "boomux-runtime-v1";
const DAEMON_STATUS_PREFIX: &[u8] = b"boomux-daemon-status-v1";
const DAEMON_PRESENCE_PREFIX: &[u8] = b"boomux-daemon-presence-v1";
const MAX_RELEASE_BYTES: u64 = 256 * 1024 * 1024;

macro_rules! remote_runtime_prefix {
    () => {
        "PATH=/usr/bin:/bin; export PATH; boomux_runtime_fail() { printf 'boomux-runtime-v1:%s:%s\\n' \"$1\" \"$2\" >&2; exit \"$2\"; }; boomux_os=$(/usr/bin/uname -s 2>/dev/null) || boomux_runtime_fail unsupported 91; boomux_uid=$(/usr/bin/id -u 2>/dev/null) || boomux_runtime_fail invalid 89; case \"$boomux_uid\" in ''|*[!0-9]*) boomux_runtime_fail invalid 89 ;; esac; if [ -n \"${XDG_RUNTIME_DIR-}\" ]; then boomux_runtime=$XDG_RUNTIME_DIR; elif [ \"$boomux_os\" = Linux ]; then boomux_runtime=/run/user/$boomux_uid; else boomux_runtime_fail missing 88; fi; case \"$boomux_runtime\" in /*) ;; *) boomux_runtime_fail invalid 89 ;; esac; [ \"${#boomux_runtime}\" -le 4096 ] || boomux_runtime_fail invalid 89; case \"$boomux_runtime\" in *[!A-Za-z0-9_./-]*) boomux_runtime_fail invalid 89 ;; esac; [ -d \"$boomux_runtime\" ] && [ ! -L \"$boomux_runtime\" ] || boomux_runtime_fail unsafe 90; case \"$boomux_os\" in Linux) boomux_runtime_stat=$(/usr/bin/stat -Lc '%u:%a' -- \"$boomux_runtime\" 2>/dev/null) || boomux_runtime_fail unsafe 90 ;; Darwin) boomux_runtime_owner=$(/usr/bin/stat -f '%u' \"$boomux_runtime\" 2>/dev/null) || boomux_runtime_fail unsafe 90; boomux_runtime_mode=$(/usr/bin/stat -f '%Lp' \"$boomux_runtime\" 2>/dev/null) || boomux_runtime_fail unsafe 90; boomux_runtime_stat=$boomux_runtime_owner:$boomux_runtime_mode ;; *) boomux_runtime_fail unsupported 91 ;; esac; [ \"$boomux_runtime_stat\" = \"$boomux_uid:700\" ] || boomux_runtime_fail unsafe 90; XDG_RUNTIME_DIR=$boomux_runtime; export XDG_RUNTIME_DIR; "
    };
}

macro_rules! remote_install_restore {
    () => {
        "sync_install_path() { boomux_sync_os=$(/usr/bin/uname -s 2>/dev/null || true); if [ \"$boomux_sync_os\" = Linux ]; then /bin/sync -f \"$1\" || return 1; fi; }; install_file_metadata() { case \"$boomux_sync_os\" in Linux) /usr/bin/stat -Lc '%u:%g:%a:%s:%Y' -- \"$1\" ;; Darwin) /usr/bin/stat -f '%u:%g:%Lp:%z:%m' \"$1\" ;; *) return 1 ;; esac; }; preserve_provisional() { [ ! -e \"$transaction/new\" ] || return 0; [ -f \"$destination\" ] && [ -x \"$destination\" ] && [ ! -L \"$destination\" ] || return 1; provisional_metadata=$(install_file_metadata \"$destination\") || return 1; provisional_size=${provisional_metadata%:*}; provisional_size=${provisional_size##*:}; case \"$provisional_size\" in ''|*[!0-9]*) return 1 ;; esac; [ \"$provisional_size\" -gt 0 ] && [ \"$provisional_size\" -le 268435456 ] || return 1; /bin/cp -p \"$destination\" \"$transaction/new\" || return 1; [ -f \"$transaction/new\" ] && [ -x \"$transaction/new\" ] && [ ! -L \"$transaction/new\" ] || return 1; [ \"$(install_file_metadata \"$destination\")\" = \"$provisional_metadata\" ] || return 1; [ \"$(install_file_metadata \"$transaction/new\")\" = \"$provisional_metadata\" ] || return 1; /usr/bin/cmp -s \"$destination\" \"$transaction/new\" || return 1; sync_install_path \"$transaction/new\" || return 1; }; restore_install() { prior_daemon=$(/bin/cat \"$transaction/prior_daemon\" 2>/dev/null || true); contacted=false; [ ! -e \"$transaction/daemon_contacted\" ] || contacted=true; if [ -e \"$transaction/restore_required\" ] && [ ! -e \"$transaction/restored\" ]; then if [ ! -e \"$transaction/new\" ]; then if [ -e \"$transaction/backup_ready\" ] && [ ! -e \"$transaction/backup\" ] && [ -f \"$destination\" ]; then :; else preserve_provisional || return 1; fi; fi; if [ -e \"$transaction/missing\" ]; then /bin/rm -f \"$destination\" || return 1; elif [ -e \"$transaction/backup_ready\" ]; then if [ -e \"$transaction/backup\" ]; then /bin/mv -f \"$transaction/backup\" \"$destination\" || return 1; else [ -e \"$transaction/new\" ] && [ -f \"$destination\" ] || return 1; fi; sync_install_path \"$destination\" || return 1; else return 1; fi; sync_install_path \"$directory\" || return 1; : > \"$transaction/restored\" || return 1; sync_install_path \"$transaction\" || return 1; fi; case \"$prior_daemon:$contacted\" in [0-9]*:true) boomux_runtime_daemon daemon restart >/dev/null 2>&1 || return 1 ;; esac; }; "
    };
}

#[allow(unused_macros)]
macro_rules! remote_claim_functions_legacy {
    () => {
        "claim_process_start() { claim_pid=$1; case \"$boomux_os\" in Linux) claim_stat=$(/bin/cat \"/proc/$claim_pid/stat\" 2>/dev/null) || return 1; claim_tail=${claim_stat##*) }; set -- $claim_tail; [ \"$#\" -ge 20 ] || return 1; [ \"$1\" != Z ] || return 1; shift 19; printf '%s' \"$1\" ;; Darwin) /bin/ps -p \"$claim_pid\" -o lstart= 2>/dev/null ;; *) return 1 ;; esac; }; claim_directory_time() { case \"$boomux_os\" in Linux) /usr/bin/stat -Lc '%Y' -- \"$lock/claim\" 2>/dev/null ;; Darwin) /usr/bin/stat -f '%m' \"$lock/claim\" 2>/dev/null ;; *) return 1 ;; esac; }; claim_release() { [ -e \"$lock/claim/ready\" ] || return 0; current_owner=$(/bin/cat \"$lock/claim/owner\" 2>/dev/null || true); [ \"$current_owner\" != \"$claim_owner\" ] || /bin/rm -rf \"$lock/claim\"; }; claim_acquire() { boomux_os=$(/usr/bin/uname -s 2>/dev/null) || return 1; claim_now=$(/bin/date +%s 2>/dev/null) || return 1; claim_pid=${claim_pid_override-$$}; claim_start=$(claim_process_start \"$claim_pid\") || return 1; claim_owner=$txn:$claim_pid:$claim_start; if ! /bin/mkdir \"$lock/claim\" 2>/dev/null; then if [ ! -e \"$lock/claim/ready\" ]; then old_created=$(/bin/cat \"$lock/claim/created\" 2>/dev/null || claim_directory_time) || return 1; case \"$old_created\" in ''|*[!0-9]*) return 1 ;; esac; claim_age=$((claim_now - old_created)); [ \"$claim_age\" -ge 180 ] || return 1; [ ! -e \"$lock/claim/ready\" ] || return 1; current_created=$(/bin/cat \"$lock/claim/created\" 2>/dev/null || claim_directory_time) || return 1; [ \"$current_created\" = \"$old_created\" ] || return 1; else old_owner=$(/bin/cat \"$lock/claim/owner\" 2>/dev/null || true); old_pid=$(/bin/cat \"$lock/claim/pid\" 2>/dev/null || true); old_start=$(/bin/cat \"$lock/claim/start\" 2>/dev/null || true); old_heartbeat=$(/bin/cat \"$lock/claim/heartbeat\" 2>/dev/null || true); case \"$old_pid:$old_heartbeat\" in *[!0-9:]*) return 1 ;; esac; claim_age=$((claim_now - old_heartbeat)); [ \"$claim_age\" -ge 180 ] || return 1; old_current=$(claim_process_start \"$old_pid\" 2>/dev/null || true); if /bin/kill -0 \"$old_pid\" 2>/dev/null && [ \"$old_current\" = \"$old_start\" ]; then return 1; fi; [ -e \"$lock/claim/ready\" ] || return 1; [ \"$(/bin/cat \"$lock/claim/owner\" 2>/dev/null || true)\" = \"$old_owner\" ] || return 1; fi; /bin/rm -rf \"$lock/claim\"; /bin/mkdir \"$lock/claim\" 2>/dev/null || return 1; fi; printf '%s\\n' \"$claim_now\" > \"$lock/claim/created\"; printf '%s\\n' \"$claim_pid\" > \"$lock/claim/pid\"; printf '%s\\n' \"$claim_start\" > \"$lock/claim/start\"; printf '%s\\n' \"$claim_now\" > \"$lock/claim/heartbeat\"; printf '%s\\n' \"$claim_owner\" > \"$lock/claim/owner\"; : > \"$lock/claim/ready\"; }; "
    };
}

macro_rules! remote_claim_functions {
    () => {
        "claim_process_start() { claim_pid=$1; case \"$boomux_os\" in Linux) claim_stat=$(/bin/cat \"/proc/$claim_pid/stat\" 2>/dev/null) || return 1; claim_tail=${claim_stat##*) }; set -- $claim_tail; [ \"$#\" -ge 20 ] || return 1; [ \"$1\" != Z ] || return 1; shift 19; printf '%s' \"$1\" ;; Darwin) /bin/ps -p \"$claim_pid\" -o lstart= 2>/dev/null ;; *) return 1 ;; esac; }; claim_release() { [ -f \"$lock/claim\" ] || return 0; IFS= read -r current_owner < \"$lock/claim\" || return 0; [ \"$current_owner\" != \"$claim_owner\" ] || /bin/rm -f \"$lock/claim\"; }; claim_acquire() { boomux_os=$(/usr/bin/uname -s 2>/dev/null) || return 1; claim_now=$(/bin/date +%s 2>/dev/null) || return 1; claim_pid=${claim_pid_override-$$}; claim_start=$(claim_process_start \"$claim_pid\") || return 1; claim_owner=$txn:$claim_pid:$claim_start; claim_record=$lock/.claim.$txn.$claim_pid; /bin/rm -f \"$claim_record\"; printf '%s\n%s\n%s\n%s\n' \"$claim_owner\" \"$claim_pid\" \"$claim_start\" \"$claim_now\" > \"$claim_record\" || return 1; if /bin/ln \"$claim_record\" \"$lock/claim\" 2>/dev/null; then /bin/rm -f \"$claim_record\"; return 0; fi; /bin/rm -f \"$claim_record\"; [ -f \"$lock/claim\" ] || return 1; { IFS= read -r old_owner; IFS= read -r old_pid; IFS= read -r old_start; IFS= read -r old_heartbeat; } < \"$lock/claim\" || return 1; case \"$old_pid:$old_heartbeat\" in *[!0-9:]*) return 1 ;; esac; claim_age=$((claim_now - old_heartbeat)); [ \"$claim_age\" -ge 180 ] || return 1; old_current=$(claim_process_start \"$old_pid\" 2>/dev/null || true); [ \"$old_current\" != \"$old_start\" ] || return 1; IFS= read -r current_owner < \"$lock/claim\" || return 1; [ \"$current_owner\" = \"$old_owner\" ] || return 1; /bin/rm -f \"$lock/claim\" || return 1; claim_acquire; }; "
    };
}

macro_rules! remote_process_identity_function {
    () => {
        "claim_process_start() { claim_pid=$1; case \"$boomux_os\" in Linux) claim_stat=$(/bin/cat \"/proc/$claim_pid/stat\" 2>/dev/null) || return 1; claim_tail=${claim_stat##*) }; set -- $claim_tail; [ \"$#\" -ge 20 ] || return 1; [ \"$1\" != Z ] || return 1; shift 19; printf '%s' \"$1\" ;; Darwin) claim_info=$(/bin/ps -p \"$claim_pid\" -o state= -o lstart= 2>/dev/null) || return 1; set -- $claim_info; [ \"$#\" -ge 6 ] || return 1; case \"$1\" in Z*) return 1 ;; esac; shift; printf '%s' \"$*\" ;; *) return 1 ;; esac; }; "
    };
}

const REMOTE_RUNTIME_PREFIX: &str = remote_runtime_prefix!();
#[doc(hidden)]
pub const REMOTE_INSTALL_COMMAND: &str = concat!(
    "boomux_runtime_daemon() ( ",
    remote_runtime_prefix!(),
    "exec \"$destination\" \"$@\"; ); ",
    "set -u; exec 3>&2; exec 2>/dev/null; umask 077; IFS= read -r txn; ",
    "stage=home; stage_code=74; transaction=; watchdog=; lock_owned=false; boomux_sync_os=$(/usr/bin/uname -s 2>/dev/null || true); ",
    remote_install_restore!(),
    remote_process_identity_function!(),
    "rollback_install() { set +e; if [ -n \"$watchdog\" ]; then kill \"$watchdog\" 2>/dev/null || true; fi; if [ -n \"$transaction\" ]; then restore_install; /bin/rm -rf \"$transaction\"; fi; if [ \"$lock_owned\" = true ]; then /bin/rm -rf \"$lock\"; fi; }; ",
    "fail_install() { trap - EXIT HUP INT TERM; set +e; printf 'boomux-install-stage-v1:%s:%s\\n' \"$stage\" \"$stage_code\" >&3; rollback_install; exit \"$stage_code\"; }; ",
    "trap fail_install EXIT; trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM; set -e; ",
    "case \"$HOME\" in /*) ;; *) exit 1 ;; esac; case \"$txn\" in .boomux.bootstrap.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) ;; *) exit 2 ;; esac; directory=$HOME/.local/bin; destination=$directory/boomux; lock=$directory/.boomux.bootstrap.lock; transaction=$directory/$txn; ",
    "stage=directory; stage_code=75; /bin/mkdir -p \"$directory\"; ",
    "stage=lock; stage_code=76; if ! /bin/mkdir \"$lock\" 2>/dev/null; then current=$(/bin/cat \"$lock/id\" 2>/dev/null || true); if [ \"$current\" = \"$txn\" ] && [ -e \"$transaction/new_ready\" ]; then /bin/cat >/dev/null; printf 'boomux-install-transaction-v1\\0%s\\0' \"$txn\"; trap - EXIT HUP INT TERM; exit; fi; trap - EXIT HUP INT TERM; exit 73; fi; lock_owned=true; ",
    "stage=transaction; stage_code=77; /bin/mkdir \"$transaction\"; ",
    "stage=lock_id; stage_code=79; printf '%s\\n' \"$txn\" > \"$lock/id\"; printf '0\\n' > \"$transaction/lease\"; temporary=$transaction/new.next; ",
    "stage=stream; stage_code=80; /bin/cat > \"$temporary\"; ",
    "stage=mode; stage_code=81; /bin/chmod 755 \"$temporary\"; upload_uid=$(/usr/bin/id -u); upload_size=$(/usr/bin/stat -Lc '%s' -- \"$temporary\" 2>/dev/null || /usr/bin/stat -f '%z' \"$temporary\"); case \"$upload_uid:$upload_size\" in *[!0-9:]*) false ;; esac; if [ \"$upload_size\" -le 0 ] || [ \"$upload_size\" -gt 268435456 ] || [ -L \"$temporary\" ] || [ ! -f \"$temporary\" ] || [ ! -x \"$temporary\" ]; then false; fi; /bin/mv \"$temporary\" \"$transaction/new\"; : > \"$transaction/new_ready\"; sync_install_path \"$transaction/new\"; ",
    "stage=watchdog_spawn; stage_code=84; ( exec 3>&-; trap '' HUP; lease_limit=180; lease_value=$(/bin/cat \"$transaction/lease\"); unchanged=0; committed=false; : > \"$transaction/watchdog_ready\"; ",
    remote_claim_functions!(),
    remote_process_identity_function!(),
    "while :; do if claim_pid_override=$(/bin/cat \"$transaction/watchdog_pid\" 2>/dev/null); then break; fi; if claim_pid_override=$(/bin/cat \"$lock/committed/watchdog_pid\" 2>/dev/null); then transaction=$lock/committed; committed=true; break; fi; [ -d \"$lock\" ] || exit; /bin/sleep 1; done; while [ -d \"$lock\" ]; do /bin/sleep 1; current=$(/bin/cat \"$transaction/lease\" 2>/dev/null || true); if [ \"$current\" != \"$lease_value\" ]; then lease_value=$current; unchanged=0; continue; fi; unchanged=$((unchanged + 1)); [ \"$unchanged\" -lt \"$lease_limit\" ] && continue; if claim_acquire; then current=$(/bin/cat \"$lock/id\" 2>/dev/null || true); claimed_lease=$(/bin/cat \"$transaction/lease\" 2>/dev/null || true); if [ \"$current\" != \"$txn\" ]; then claim_release; exit; fi; if [ \"$claimed_lease\" != \"$lease_value\" ]; then claim_release; lease_value=$claimed_lease; unchanged=0; continue; fi; if ! $committed && ! restore_install; then claim_release; unchanged=0; continue; fi; /bin/rm -rf \"$transaction\" \"$lock\"; exit; fi; unchanged=0; done ) </dev/null >/dev/null 2>&1 & watchdog=$!; ",
    "stage=watchdog_ready; stage_code=85; attempts=0; while [ ! -e \"$transaction/watchdog_ready\" ]; do kill -0 \"$watchdog\"; attempts=$((attempts + 1)); [ \"$attempts\" -lt 1000 ]; /bin/sleep 0.01; done; ",
    "stage=watchdog_pid; stage_code=86; boomux_os=$(/usr/bin/uname -s); watchdog_start=$(claim_process_start \"$watchdog\"); printf '%s\\n' \"$watchdog_start\" > \"$transaction/watchdog_start.next\"; /bin/mv -f \"$transaction/watchdog_start.next\" \"$transaction/watchdog_start\"; printf '%s\\n' \"$watchdog\" > \"$transaction/watchdog_pid.next\"; /bin/mv -f \"$transaction/watchdog_pid.next\" \"$transaction/watchdog_pid\"; sync_install_path \"$transaction\"; rm -f \"$transaction/watchdog_ready\"; ",
    "stage=result; stage_code=87; printf 'boomux-install-transaction-v1\\0%s\\0' \"$txn\"; ",
    "trap - EXIT HUP INT TERM; exec 3>&-"
);
#[allow(dead_code)]
const REMOTE_INSTALL_ACTIVATE_COMMAND_LEGACY: &str = concat!(
    "boomux_runtime_daemon() ( ",
    remote_runtime_prefix!(),
    "exec \"$destination\" \"$@\"; ); ",
    "set -eu; exec 3>&2; exec 2>/dev/null; umask 077; IFS= read -r txn; IFS= read -r reason; IFS= read -r prior; case \"$HOME\" in /*) ;; *) exit 1 ;; esac; case \"$txn\" in .boomux.bootstrap.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) ;; *) exit 2 ;; esac; case \"$reason:$prior\" in missing:absent|upgrade:[0-9]*) ;; *) exit 2 ;; esac; directory=$HOME/.local/bin; destination=$directory/boomux; lock=$directory/.boomux.bootstrap.lock; transaction=$directory/$txn; backup=$transaction/backup; ",
    "[ \"$(/bin/cat \"$lock/id\")\" = \"$txn\" ]; if ! /bin/mkdir \"$lock/claim\" 2>/dev/null; then exit 73; fi; trap '/bin/rmdir \"$lock/claim\" 2>/dev/null || true' EXIT HUP INT TERM; if [ -e \"$transaction/activated\" ]; then trap - EXIT HUP INT TERM; /bin/rmdir \"$lock/claim\"; printf 'boomux-install-activation-v1\\0activated\\0'; exit; fi; if [ ! -e \"$transaction/new_ready\" ] || [ ! -f \"$transaction/new\" ] || [ ! -x \"$transaction/new\" ] || [ -L \"$transaction/new\" ]; then false; fi; ",
    "if [ \"$reason\" = missing ]; then ",
    remote_runtime_prefix!(),
    "socket=$XDG_RUNTIME_DIR/boomux/daemon.sock; if [ -S \"$socket\" ] || [ -e \"$socket\" ] || [ -L \"$socket\" ]; then trap - EXIT HUP INT TERM; /bin/rmdir \"$lock/claim\"; printf 'boomux-install-activation-v1\\0daemon_present\\0'; exit; fi; fi; ",
    "sync_install_path() { boomux_sync_os=$(/usr/bin/uname -s 2>/dev/null || true); if [ \"$boomux_sync_os\" = Linux ]; then /bin/sync -f \"$1\"; fi; }; restore_activation() { if [ -e \"$transaction/restore_required\" ]; then if [ -e \"$transaction/missing\" ]; then /bin/rm -f \"$destination\"; elif [ -e \"$transaction/backup_ready\" ]; then /bin/mv -f \"$backup\" \"$destination\"; fi; sync_install_path \"$directory\"; fi; }; fail_activation() { code=$?; trap - EXIT HUP INT TERM; set +e; restore_activation; /bin/rmdir \"$lock/claim\" 2>/dev/null || true; exit \"$code\"; }; trap fail_activation EXIT HUP INT TERM; install_os=$(/usr/bin/uname -s); install_uid=$(/usr/bin/id -u); case \"$install_uid\" in ''|*[!0-9]*) false ;; esac; case \"$install_os\" in Linux) install_metadata() { /usr/bin/stat -Lc '%u:%g:%a:%s:%Y' -- \"$1\"; }; install_size() { /usr/bin/stat -Lc '%s' -- \"$1\"; } ;; Darwin) install_metadata() { /usr/bin/stat -f '%u:%g:%Lp:%z:%m' \"$1\"; }; install_size() { /usr/bin/stat -f '%z' \"$1\"; } ;; *) false ;; esac; if [ -e \"$destination\" ] || [ -L \"$destination\" ]; then if [ -L \"$destination\" ] || [ ! -f \"$destination\" ] || [ ! -x \"$destination\" ]; then false; fi; destination_metadata=$(install_metadata \"$destination\"); destination_owner=${destination_metadata%%:*}; [ \"$destination_owner\" = \"$install_uid\" ]; destination_size=$(install_size \"$destination\"); case \"$destination_size\" in ''|*[!0-9]*) false ;; esac; if [ \"$destination_size\" -le 0 ] || [ \"$destination_size\" -gt 268435456 ]; then false; fi; /bin/cp -p \"$destination\" \"$backup\"; if [ -L \"$backup\" ] || [ ! -f \"$backup\" ] || [ ! -x \"$backup\" ]; then false; fi; [ \"$(install_metadata \"$destination\")\" = \"$destination_metadata\" ]; [ \"$(install_metadata \"$backup\")\" = \"$destination_metadata\" ]; /usr/bin/cmp -s \"$destination\" \"$backup\"; sync_install_path \"$backup\"; : > \"$transaction/backup_ready\"; else : > \"$transaction/missing\"; fi; printf '%s\\n' \"$prior\" > \"$transaction/prior_daemon\"; : > \"$transaction/restore_required\"; /bin/mv -f \"$transaction/new\" \"$destination\"; sync_install_path \"$destination\"; sync_install_path \"$directory\"; : > \"$transaction/activated\"; trap - EXIT HUP INT TERM; /bin/rmdir \"$lock/claim\"; printf 'boomux-install-activation-v1\\0activated\\0'"
);
#[doc(hidden)]
pub const REMOTE_INSTALL_ACTIVATE_COMMAND: &str = concat!(
    "set -eu; exec 3>&2; exec 2>/dev/null; umask 077; ",
    "IFS= read -r txn; IFS= read -r reason; IFS= read -r prior; IFS= read -r proof_pid; IFS= read -r proof_executable; IFS= read -r proof_device; IFS= read -r proof_inode; ",
    "case \"$HOME\" in /*) ;; *) exit 1 ;; esac; case \"$txn\" in .boomux.bootstrap.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) ;; *) exit 2 ;; esac; case \"$reason:$prior\" in missing:absent|upgrade:[0-9]*) ;; *) exit 2 ;; esac; ",
    "directory=$HOME/.local/bin; destination=$directory/boomux; lock=$directory/.boomux.bootstrap.lock; transaction=$directory/$txn; backup=$transaction/backup; [ \"$(/bin/cat \"$lock/id\")\" = \"$txn\" ]; ",
    remote_claim_functions!(),
    remote_process_identity_function!(),
    "claim_acquire || exit 73; trap claim_release EXIT HUP INT TERM; ",
    "sync_install_path() { boomux_sync_os=$(/usr/bin/uname -s 2>/dev/null || true); if [ \"$boomux_sync_os\" = Linux ]; then /bin/sync -f \"$1\" || return 1; fi; }; if [ -e \"$transaction/activated\" ] && [ -e \"$transaction/restore_required\" ] && [ ! -e \"$transaction/new\" ] && [ -f \"$destination\" ] && [ -x \"$destination\" ] && [ ! -L \"$destination\" ] && { { [ -e \"$transaction/missing\" ] && [ ! -e \"$transaction/backup_ready\" ]; } || { [ -e \"$transaction/backup_ready\" ] && [ ! -e \"$transaction/missing\" ] && [ -f \"$backup\" ]; }; }; then sync_install_path \"$destination\"; sync_install_path \"$directory\"; sync_install_path \"$transaction\"; trap - EXIT HUP INT TERM; claim_release; printf 'boomux-install-activation-v1\\0activated\\0'; exit; fi; ",
    "sync_install_path() { boomux_sync_os=$(/usr/bin/uname -s 2>/dev/null || true); if [ \"$boomux_sync_os\" = Linux ]; then /bin/sync -f \"$1\" || return 1; fi; }; restore_activation() { [ -e \"$transaction/restore_required\" ] || return 0; if [ ! -e \"$transaction/restored\" ]; then if [ ! -e \"$transaction/new\" ]; then if [ -e \"$transaction/backup_ready\" ] && [ ! -e \"$backup\" ] && [ -f \"$destination\" ]; then :; else /bin/mv -f \"$destination\" \"$transaction/new\" || return 1; sync_install_path \"$transaction/new\" || return 1; fi; fi; [ -f \"$transaction/new\" ] && [ -x \"$transaction/new\" ] && [ ! -L \"$transaction/new\" ] || return 1; if [ -e \"$transaction/missing\" ]; then /bin/rm -f \"$destination\" || return 1; elif [ -e \"$transaction/backup_ready\" ]; then if [ -e \"$backup\" ]; then /bin/mv -f \"$backup\" \"$destination\" || return 1; else [ -f \"$destination\" ] || return 1; fi; sync_install_path \"$destination\" || return 1; else return 1; fi; sync_install_path \"$directory\" || return 1; : > \"$transaction/restored\" || return 1; sync_install_path \"$transaction\" || return 1; fi; /bin/rm -f \"$transaction/activated\" \"$transaction/restore_required\" \"$transaction/backup_ready\" \"$transaction/missing\" \"$transaction/restored\" || return 1; sync_install_path \"$transaction\" || return 1; }; fail_activation() { code=$?; trap - EXIT HUP INT TERM; set +e; restore_activation; claim_release; exit \"$code\"; }; trap fail_activation EXIT HUP INT TERM; if [ -e \"$transaction/restore_required\" ]; then restore_activation; fi; [ -e \"$transaction/new_ready\" ] && [ -f \"$transaction/new\" ] && [ -x \"$transaction/new\" ] && [ ! -L \"$transaction/new\" ]; /bin/rm -f \"$backup\" \"$transaction/backup_ready\" \"$transaction/missing\" \"$transaction/restored\"; ",
    "install_os=$(/usr/bin/uname -s); install_uid=$(/usr/bin/id -u); case \"$install_uid\" in ''|*[!0-9]*) false ;; esac; case \"$install_os\" in Linux) install_metadata() { /usr/bin/stat -Lc '%u:%g:%a:%s:%Y' -- \"$1\"; }; install_size() { /usr/bin/stat -Lc '%s' -- \"$1\"; } ;; Darwin) install_metadata() { /usr/bin/stat -f '%u:%g:%Lp:%z:%m' \"$1\"; }; install_size() { /usr/bin/stat -f '%z' \"$1\"; } ;; *) false ;; esac; if [ -e \"$destination\" ] || [ -L \"$destination\" ]; then if [ -L \"$destination\" ] || [ ! -f \"$destination\" ] || [ ! -x \"$destination\" ]; then false; fi; destination_metadata=$(install_metadata \"$destination\"); destination_owner=${destination_metadata%%:*}; [ \"$destination_owner\" = \"$install_uid\" ]; destination_size=$(install_size \"$destination\"); case \"$destination_size\" in ''|*[!0-9]*) false ;; esac; [ \"$destination_size\" -gt 0 ] && [ \"$destination_size\" -le 268435456 ]; /bin/cp -p \"$destination\" \"$backup\"; [ -f \"$backup\" ] && [ -x \"$backup\" ] && [ ! -L \"$backup\" ]; [ \"$(install_metadata \"$destination\")\" = \"$destination_metadata\" ]; [ \"$(install_metadata \"$backup\")\" = \"$destination_metadata\" ]; /usr/bin/cmp -s \"$destination\" \"$backup\"; sync_install_path \"$backup\"; : > \"$transaction/backup_ready\"; else : > \"$transaction/missing\"; fi; printf '%s\\n' \"$prior\" > \"$transaction/prior_daemon\"; : > \"$transaction/restore_required\"; sync_install_path \"$transaction\"; ",
    remote_runtime_prefix!(),
    "if [ \"$reason\" = missing ]; then socket=$XDG_RUNTIME_DIR/boomux/daemon.sock; if [ -S \"$socket\" ] || [ -e \"$socket\" ] || [ -L \"$socket\" ]; then restore_activation; trap - EXIT HUP INT TERM; claim_release; printf 'boomux-install-activation-v1\\0daemon_present\\0'; exit; fi; /bin/mv -f \"$transaction/new\" \"$destination\"; : > \"$transaction/activated\"; else case \"$proof_pid:$proof_device:$proof_inode\" in *[!0-9:]*) false ;; esac; [ \"$proof_executable\" = \"$destination\" ]; \"$transaction/new\" __bootstrap-activate \"$txn\" \"$proof_pid\" \"$prior\" \"$proof_executable\" \"$proof_device\" \"$proof_inode\"; fi; ",
    "trap - EXIT HUP INT TERM; claim_release; printf 'boomux-install-activation-v1\\0activated\\0'"
);
#[doc(hidden)]
#[allow(dead_code)]
const REMOTE_INSTALL_ROLLBACK_COMMAND_LEGACY: &str = concat!(
    "boomux_runtime_daemon() ( ",
    remote_runtime_prefix!(),
    "exec \"$destination\" \"$@\"; ); ",
    "set -eu; case \"$HOME\" in /*) ;; *) exit 1 ;; esac; IFS= read -r txn; case \"$txn\" in .boomux.bootstrap.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) ;; *) exit 2 ;; esac; directory=$HOME/.local/bin; destination=$directory/boomux; lock=$directory/.boomux.bootstrap.lock; transaction=$directory/$txn; [ \"$(/bin/cat \"$lock/id\")\" = \"$txn\" ]; /bin/mkdir \"$lock/claim\"; trap '/bin/rmdir \"$lock/claim\" 2>/dev/null || true' EXIT HUP INT TERM; watchdog=$(/bin/cat \"$transaction/watchdog_pid\" 2>/dev/null || true); ",
    remote_install_restore!(),
    "case \"$watchdog\" in *[!0-9]*|'') ;; *) kill \"$watchdog\" 2>/dev/null || true ;; esac; restore_install; trap - EXIT HUP INT TERM; /bin/rm -rf \"$transaction\" \"$lock\""
);
#[doc(hidden)]
pub const REMOTE_INSTALL_ROLLBACK_COMMAND: &str = concat!(
    "PATH=/usr/bin:/bin; export PATH; boomux_runtime_daemon() ( ",
    remote_runtime_prefix!(),
    "exec \"$destination\" \"$@\"; ); set -eu; case \"$HOME\" in /*) ;; *) exit 1 ;; esac; IFS= read -r txn; case \"$txn\" in .boomux.bootstrap.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) ;; *) exit 2 ;; esac; directory=$HOME/.local/bin; destination=$directory/boomux; lock=$directory/.boomux.bootstrap.lock; transaction=$directory/$txn; boomux_sync_os=$(/usr/bin/uname -s 2>/dev/null || true); [ \"$(/bin/cat \"$lock/id\")\" = \"$txn\" ]; ",
    remote_claim_functions!(),
    remote_process_identity_function!(),
    "claim_acquire || exit 73; trap claim_release EXIT HUP INT TERM; watchdog=$(/bin/cat \"$transaction/watchdog_pid\" 2>/dev/null || true); watchdog_start=$(/bin/cat \"$transaction/watchdog_start\" 2>/dev/null || true); ",
    remote_install_restore!(),
    "restore_install; case \"$watchdog\" in *[!0-9]*|'') ;; *) current_watchdog_start=$(claim_process_start \"$watchdog\" 2>/dev/null || true); if [ -n \"$watchdog_start\" ] && [ \"$current_watchdog_start\" = \"$watchdog_start\" ]; then /bin/kill \"$watchdog\" 2>/dev/null || true; fi ;; esac; trap - EXIT HUP INT TERM; /bin/rm -rf \"$transaction\" \"$lock\""
);
#[allow(dead_code)]
const REMOTE_INSTALL_COMMIT_COMMAND_LEGACY: &str = "set -eu; case \"$HOME\" in /*) ;; *) exit 1 ;; esac; IFS= read -r txn; case \"$txn\" in .boomux.bootstrap.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) ;; *) exit 2 ;; esac; directory=$HOME/.local/bin; lock=$directory/.boomux.bootstrap.lock; transaction=$directory/$txn; committed=$lock/committed; [ \"$(cat \"$lock/id\")\" = \"$txn\" ]; ( trap '' HUP; sleep 180; if [ \"$(cat \"$lock/id\" 2>/dev/null || true)\" = \"$txn\" ]; then rm -rf \"$lock/claim\"; fi ) </dev/null >/dev/null 2>&1 & if [ -d \"$committed\" ]; then printf 'boomux-install-commit-v1\\0committed\\0'; exit; fi; mkdir \"$lock/claim\"; trap 'rmdir \"$lock/claim\" 2>/dev/null || true' EXIT HUP INT TERM; if [ -d \"$committed\" ]; then :; elif [ -d \"$transaction\" ]; then mv \"$transaction\" \"$committed\"; else exit 1; fi; trap - EXIT HUP INT TERM; rmdir \"$lock/claim\"; printf 'boomux-install-commit-v1\\0committed\\0'";
const REMOTE_INSTALL_COMMIT_COMMAND: &str = concat!(
    "PATH=/usr/bin:/bin; export PATH; set -eu; case \"$HOME\" in /*) ;; *) exit 1 ;; esac; IFS= read -r txn; case \"$txn\" in .boomux.bootstrap.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) ;; *) exit 2 ;; esac; directory=$HOME/.local/bin; lock=$directory/.boomux.bootstrap.lock; transaction=$directory/$txn; committed=$lock/committed; [ \"$(/bin/cat \"$lock/id\")\" = \"$txn\" ]; ",
    remote_claim_functions!(),
    remote_process_identity_function!(),
    "if [ -d \"$committed\" ]; then printf 'boomux-install-commit-v1\\0committed\\0'; exit; fi; claim_acquire || exit 73; trap claim_release EXIT HUP INT TERM; if [ -d \"$committed\" ]; then :; elif [ -d \"$transaction\" ]; then /bin/mv \"$transaction\" \"$committed\"; else exit 1; fi; trap - EXIT HUP INT TERM; claim_release; printf 'boomux-install-commit-v1\\0committed\\0'"
);
#[doc(hidden)]
pub const REMOTE_INSTALL_MARK_RESTARTED_COMMAND: &str = "set -eu; case \"$HOME\" in /*) ;; *) exit 1 ;; esac; IFS= read -r txn; case \"$txn\" in .boomux.bootstrap.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) ;; *) exit 2 ;; esac; directory=$HOME/.local/bin; lock=$directory/.boomux.bootstrap.lock; transaction=$directory/$txn; [ \"$(cat \"$lock/id\")\" = \"$txn\" ]; : > \"$transaction/restarted\"";
const REMOTE_INSTALL_MARK_DAEMON_CONTACTED_COMMAND: &str = "set -eu; case \"$HOME\" in /*) ;; *) exit 1 ;; esac; IFS= read -r txn; case \"$txn\" in .boomux.bootstrap.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) ;; *) exit 2 ;; esac; directory=$HOME/.local/bin; lock=$directory/.boomux.bootstrap.lock; transaction=$directory/$txn; [ \"$(cat \"$lock/id\")\" = \"$txn\" ]; : > \"$transaction/daemon_contacted\"";
#[allow(dead_code)]
const REMOTE_INSTALL_RENEW_COMMAND_LEGACY: &str = "set -eu; case \"$HOME\" in /*) ;; *) exit 1 ;; esac; IFS= read -r txn; IFS= read -r renewal; case \"$txn\" in .boomux.bootstrap.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) ;; *) exit 2 ;; esac; case \"$renewal\" in ''|*[!0-9]*) exit 2 ;; esac; directory=$HOME/.local/bin; lock=$directory/.boomux.bootstrap.lock; transaction=$directory/$txn; temporary=$transaction/lease.next.$$; printf '%s\\n' \"$renewal\" > \"$temporary\"; if ! /bin/mkdir \"$lock/claim\" 2>/dev/null; then /bin/rm -f \"$temporary\"; exit 3; fi; trap '/bin/rm -f \"$temporary\"; /bin/rmdir \"$lock/claim\" 2>/dev/null || true' EXIT HUP INT TERM; [ \"$(/bin/cat \"$lock/id\")\" = \"$txn\" ]; /bin/mv -f \"$temporary\" \"$transaction/lease\"; trap - EXIT HUP INT TERM; /bin/rmdir \"$lock/claim\"";
const REMOTE_INSTALL_RENEW_COMMAND: &str = concat!(
    "PATH=/usr/bin:/bin; export PATH; set -eu; case \"$HOME\" in /*) ;; *) exit 1 ;; esac; IFS= read -r txn; IFS= read -r renewal; case \"$txn\" in .boomux.bootstrap.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) ;; *) exit 2 ;; esac; case \"$renewal\" in ''|*[!0-9]*) exit 2 ;; esac; directory=$HOME/.local/bin; lock=$directory/.boomux.bootstrap.lock; transaction=$directory/$txn; temporary=$transaction/lease.next.$$; printf '%s\\n' \"$renewal\" > \"$temporary\"; ",
    remote_claim_functions!(),
    remote_process_identity_function!(),
    "if [ -e \"$lock/claim\" ] || ! claim_acquire; then /bin/rm -f \"$temporary\"; exit 3; fi; trap '/bin/rm -f \"$temporary\"; claim_release' EXIT HUP INT TERM; [ \"$(/bin/cat \"$lock/id\")\" = \"$txn\" ]; /bin/mv -f \"$temporary\" \"$transaction/lease\"; trap - EXIT HUP INT TERM; claim_release"
);

pub const PLATFORM_PROBE_COMMAND: &str = "os=$(/usr/bin/uname -s) || exit 92; arch=$(/usr/bin/uname -m) || exit 92; case \"$os\" in Linux|Darwin) ;; *) exit 92 ;; esac; for command in /usr/bin/uname /usr/bin/id /usr/bin/stat /usr/bin/cmp /bin/cat /bin/chmod /bin/cp /bin/date /bin/kill /bin/ln /bin/mkdir /bin/mv /bin/ps /bin/rm /bin/rmdir /bin/sleep /bin/sync; do [ -x \"$command\" ] || exit 92; done; printf 'boomux-platform-v1\\0%s\\0%s\\0' \"$os\" \"$arch\"";
pub const EXECUTABLE_PROBE_COMMAND: &str = "printf 'boomux-executables-v1\\0'; path=$(command -v boomux 2>/dev/null || true); for candidate in \"$path\" /usr/local/bin/boomux /usr/bin/boomux /opt/homebrew/bin/boomux /home/linuxbrew/.linuxbrew/bin/boomux \"$HOME/.local/bin/boomux\" \"$HOME/.local/share/mise/shims/boomux\" \"$HOME/.nix-profile/bin/boomux\" /run/current-system/sw/bin/boomux; do case \"$candidate\" in /*) if [ \"$candidate\" = \"$HOME/.local/bin/boomux\" ] && [ -d \"$HOME/.local/bin/.boomux.bootstrap.lock\" ] && [ ! -d \"$HOME/.local/bin/.boomux.bootstrap.lock/committed\" ]; then continue; fi; [ -f \"$candidate\" ] && [ -x \"$candidate\" ] && printf '%s\\0' \"$candidate\" ;; esac; done; true";
pub const INSTALL_DESTINATION_PROBE_COMMAND: &str = "case \"$HOME\" in /*) destination=$HOME/.local/bin/boomux; lock=$HOME/.local/bin/.boomux.bootstrap.lock; state=clear; if [ -d \"$lock\" ] && [ ! -d \"$lock/committed\" ]; then state=stale; txn=$(/bin/cat \"$lock/id\" 2>/dev/null || true); case \"$txn\" in .boomux.bootstrap.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) transaction=$HOME/.local/bin/$txn; watchdog=$(/bin/cat \"$transaction/watchdog_pid\" 2>/dev/null || true); expected_start=$(/bin/cat \"$transaction/watchdog_start\" 2>/dev/null || true); case \"$watchdog\" in ''|*[!0-9]*) ;; *) probe_os=$(/usr/bin/uname -s 2>/dev/null || true); case \"$probe_os\" in Linux) watchdog_stat=$(/bin/cat \"/proc/$watchdog/stat\" 2>/dev/null || true); watchdog_tail=${watchdog_stat##*) }; set -- $watchdog_tail; if [ \"$#\" -ge 20 ] && [ \"$1\" != Z ]; then shift 19; current_start=$1; else current_start=; fi ;; Darwin) watchdog_info=$(/bin/ps -p \"$watchdog\" -o state= -o lstart= 2>/dev/null || true); set -- $watchdog_info; if [ \"$#\" -ge 6 ]; then case \"$1\" in Z*) current_start= ;; *) shift; current_start=$* ;; esac; else current_start=; fi ;; *) current_start= ;; esac; if [ -n \"$expected_start\" ] && [ \"$current_start\" = \"$expected_start\" ]; then state=recovering; fi ;; esac ;; esac; fi; printf 'boomux-install-destination-v1\\0%s\\0%s\\0' \"$destination\" \"$state\" ;; *) exit 1 ;; esac";

fn remote_daemon_command(executable: &RemoteExecutable, arguments: &str) -> String {
    format!(
        "{REMOTE_RUNTIME_PREFIX}exec {} {arguments}",
        quote_posix_shell(executable.as_str())
    )
}

fn remote_installed_daemon_command(arguments: &str) -> String {
    format!(
        "case \"$HOME\" in /*) ;; *) exit 89 ;; esac; {REMOTE_RUNTIME_PREFIX}exec \"$HOME/.local/bin/boomux\" {arguments}"
    )
}

fn remote_daemon_status_command() -> String {
    format!(
        "case \"$HOME\" in /*) ;; *) exit 89 ;; esac; {REMOTE_RUNTIME_PREFIX}if [ ! -S \"$XDG_RUNTIME_DIR/boomux/daemon.sock\" ]; then printf 'boomux-daemon-status-v1\\0absent\\0'; exit; fi; exec \"$HOME/.local/bin/boomux\" daemon status --json"
    )
}

fn remote_daemon_status_command_for(executable: &RemoteExecutable) -> String {
    if executable.as_str().ends_with("/.local/bin/boomux") {
        return remote_daemon_status_command();
    }
    format!(
        "{REMOTE_RUNTIME_PREFIX}if [ ! -S \"$XDG_RUNTIME_DIR/boomux/daemon.sock\" ]; then printf 'boomux-daemon-status-v1\\0absent\\0'; exit; fi; exec {} daemon status --json",
        quote_posix_shell(executable.as_str())
    )
}

fn remote_transaction_daemon_status_command(transaction: &InstallTransactionId) -> String {
    format!(
        "case \"$HOME\" in /*) ;; *) exit 89 ;; esac; {REMOTE_RUNTIME_PREFIX}exec \"$HOME/.local/bin/{}/new\" daemon status --json",
        transaction.0
    )
}

fn remote_daemon_presence_command() -> String {
    format!(
        "{REMOTE_RUNTIME_PREFIX}socket=$XDG_RUNTIME_DIR/boomux/daemon.sock; if [ -S \"$socket\" ] || [ -e \"$socket\" ] || [ -L \"$socket\" ]; then printf 'boomux-daemon-presence-v1\\0present\\0'; else printf 'boomux-daemon-presence-v1\\0absent\\0'; fi"
    )
}

fn remote_helper_command(executable: &RemoteExecutable) -> String {
    remote_daemon_command(executable, "__federation-stdio")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshAuthenticationMode {
    Interactive,
    Batch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteProbe {
    Platform,
    Executables,
    InstallDestination,
}

impl RemoteProbe {
    pub const fn command(self) -> &'static str {
        match self {
            Self::Platform => PLATFORM_PROBE_COMMAND,
            Self::Executables => EXECUTABLE_PROBE_COMMAND,
            Self::InstallDestination => INSTALL_DESTINATION_PROBE_COMMAND,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOperatingSystem {
    Linux,
    MacOs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteArchitecture {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemotePlatform {
    pub operating_system: RemoteOperatingSystem,
    pub architecture: RemoteArchitecture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDiscovery {
    pub platform: RemotePlatform,
    pub executables: Vec<RemoteExecutable>,
    pub install_destination: RemoteExecutable,
    recovery: RemoteRecoveryState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteRecoveryState {
    Clear,
    Active,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatibleRemoteHelper {
    pub executable: RemoteExecutable,
    pub handshake: FederationHandshake,
    bootstrap_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteInstallSource {
    CurrentBinary {
        path: PathBuf,
        sha256: String,
        bytes: Vec<u8>,
    },
    Release {
        target: &'static str,
        tag: String,
        sha256: String,
        bytes: Vec<u8>,
    },
}

impl RemoteInstallSource {
    pub fn description(&self) -> String {
        match self {
            Self::CurrentBinary { path, sha256, .. } => {
                format!(
                    "ABI-unverified pinned current binary {} (sha256 {sha256})",
                    path.display()
                )
            }
            Self::Release {
                target,
                tag,
                sha256,
                ..
            } => {
                format!(
                    "pinned checksum-verified GitHub release {tag} for {target} (sha256 {sha256})"
                )
            }
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Self::CurrentBinary { bytes, .. } | Self::Release { bytes, .. } => bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInstallPlan {
    pub target: SshTarget,
    pub destination: RemoteExecutable,
    pub source: RemoteInstallSource,
    pub reason: RemoteInstallReason,
    bootstrap_id: Option<Uuid>,
    upgrade_helper: Option<RemoteExecutable>,
    intent: RemoteInstallIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteInstallIntent {
    AutomaticCompatibility,
    ExplicitRegisteredUpgrade { expected_node_id: String },
}

impl RemoteInstallIntent {
    fn verify_helper_identity(&self, helper: &CompatibleRemoteHelper) -> io::Result<()> {
        let Self::ExplicitRegisteredUpgrade { expected_node_id } = self else {
            return Ok(());
        };
        if helper.handshake.node_id == *expected_node_id {
            Ok(())
        } else {
            Err(classified_error(
                io::ErrorKind::PermissionDenied,
                "node_identity_changed",
                "remote helper identity changed from the registered Node",
            ))
        }
    }

    fn daemon_restart_required(&self, status: &RemoteDaemonStatus) -> bool {
        match self {
            Self::AutomaticCompatibility => status.restart_required(),
            Self::ExplicitRegisteredUpgrade { .. } => {
                matches!(status, RemoteDaemonStatus::Present { .. })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteInstallReason {
    Missing,
    Upgrade,
}

#[derive(Debug)]
struct ClassifiedBootstrapError {
    code: &'static str,
    message: String,
    recovery: BootstrapRecoveryDisposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapRecoveryDisposition {
    NoRemoteMutation,
    RollbackConfirmed,
    OutcomeUnknown,
}

impl std::fmt::Display for ClassifiedBootstrapError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClassifiedBootstrapError {}

fn classified_error(
    kind: io::ErrorKind,
    code: &'static str,
    message: impl Into<String>,
) -> io::Error {
    classified_error_with_recovery(
        kind,
        code,
        message,
        BootstrapRecoveryDisposition::NoRemoteMutation,
    )
}

fn classified_error_with_recovery(
    kind: io::ErrorKind,
    code: &'static str,
    message: impl Into<String>,
    recovery: BootstrapRecoveryDisposition,
) -> io::Error {
    io::Error::new(
        kind,
        ClassifiedBootstrapError {
            code,
            message: message.into(),
            recovery,
        },
    )
}

fn with_recovery(error: io::Error, recovery: BootstrapRecoveryDisposition) -> io::Error {
    classified_error_with_recovery(
        error.kind(),
        error_code(&error),
        error.to_string(),
        recovery,
    )
}

fn post_install_failure(stage: &'static str, error: io::Error) -> io::Error {
    if matches!(
        error_code(&error),
        "bootstrap_runtime_unavailable" | "node_identity_changed"
    ) {
        return with_recovery(error, BootstrapRecoveryDisposition::OutcomeUnknown);
    }
    let message = match stage {
        "stream" => "remote provisional executable upload failed",
        "activation" => "remote guarded executable activation failed",
        "daemon_status" => "remote post-install daemon status verification failed",
        "daemon_restart" => "remote post-install graceful daemon restart failed",
        "helper_verification" => "remote post-install helper verification failed",
        "live_handshake" => "remote post-install live helper handshake failed",
        "protocol_ping" => "remote post-install protocol ping failed",
        _ => "remote post-install verification failed",
    };
    let transport = matches!(
        error.kind(),
        io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
    );
    classified_error_with_recovery(
        error.kind(),
        if transport {
            "bootstrap_transport_failed"
        } else {
            "bootstrap_install_failed"
        },
        message,
        BootstrapRecoveryDisposition::OutcomeUnknown,
    )
}

fn post_contact_failure(
    stage: &'static str,
    error: io::Error,
    reason: RemoteInstallReason,
) -> io::Error {
    let error = post_install_failure(stage, error);
    let message = if reason == RemoteInstallReason::Missing {
        format!(
            "{error}; filesystem rollback does not stop runtime processes, so a daemon started independently during bootstrap may remain and require manual recovery"
        )
    } else {
        error.to_string()
    };
    classified_error_with_recovery(
        error.kind(),
        error_code(&error),
        message,
        BootstrapRecoveryDisposition::RollbackConfirmed,
    )
}

pub fn error_code(error: &io::Error) -> &'static str {
    if let Some(classified) = error
        .get_ref()
        .and_then(|error| error.downcast_ref::<ClassifiedBootstrapError>())
    {
        return classified.code;
    }
    let message = error.to_string();
    match error.kind() {
        io::ErrorKind::PermissionDenied if message.contains("different Node identities") => {
            "node_identity_conflict"
        }
        io::ErrorKind::PermissionDenied if message.contains("identity changed") => {
            "node_identity_changed"
        }
        io::ErrorKind::PermissionDenied => "bootstrap_authentication_failed",
        io::ErrorKind::TimedOut
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::NotConnected
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::UnexpectedEof => "bootstrap_transport_failed",
        io::ErrorKind::Unsupported if message.contains("platform") => {
            "bootstrap_unsupported_platform"
        }
        io::ErrorKind::Unsupported => "unsupported_version",
        io::ErrorKind::InvalidData
            if message.contains("platform")
                || message.contains("operating system")
                || message.contains("architecture")
                || message.contains("release asset") =>
        {
            "bootstrap_unsupported_platform"
        }
        io::ErrorKind::InvalidData if message.contains("newer incompatible") => {
            "unsupported_version"
        }
        io::ErrorKind::InvalidData if message.contains("install source") => {
            "bootstrap_install_failed"
        }
        io::ErrorKind::InvalidData => "bootstrap_malformed_helper",
        io::ErrorKind::Other
            if message.contains("install")
                || message.contains("rollback")
                || message.contains("release asset") =>
        {
            "bootstrap_install_failed"
        }
        io::ErrorKind::Other => "bootstrap_transport_failed",
        _ => "bootstrap_transport_failed",
    }
}

pub fn recovery_disposition(error: &io::Error) -> BootstrapRecoveryDisposition {
    error
        .get_ref()
        .and_then(|error| error.downcast_ref::<ClassifiedBootstrapError>())
        .map(|error| error.recovery)
        .unwrap_or(BootstrapRecoveryDisposition::OutcomeUnknown)
}

impl RemoteInstallReason {
    pub const fn error_code(self) -> &'static str {
        match self {
            Self::Missing => "install_required",
            Self::Upgrade => "upgrade_required",
        }
    }

    pub const fn noninteractive_message(self) -> &'static str {
        match self {
            Self::Missing => {
                "remote Boomux installation is required; rerun `boomux node add ALIAS TARGET` in an interactive terminal to review and authorize it"
            }
            Self::Upgrade => {
                "remote Boomux is outdated and must be upgraded; rerun `boomux node add ALIAS TARGET` in an interactive terminal to review and authorize it"
            }
        }
    }
}

pub enum RemoteBootstrapPlan {
    Ready(CompatibleRemoteHelper),
    Install(RemoteInstallPlan),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallTransactionId(String);

impl InstallTransactionId {
    fn generate() -> Self {
        let simple = Uuid::new_v4().simple().to_string();
        Self(format!(".boomux.bootstrap.{}", &simple[..8]))
    }

    fn parse_probe(output: &[u8]) -> io::Result<Self> {
        let fields = parse_nul_fields(output, INSTALL_TRANSACTION_PREFIX)?;
        if fields.len() != 1 {
            return Err(invalid_probe(
                "remote install returned an invalid transaction field count",
            ));
        }
        let value = std::str::from_utf8(fields[0])
            .map_err(|_| invalid_probe("remote install returned a non-UTF-8 transaction ID"))?;
        let suffix = value
            .strip_prefix(".boomux.bootstrap.")
            .filter(|suffix| {
                suffix.len() == 8 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
            .ok_or_else(|| invalid_probe("remote install returned an invalid transaction ID"))?;
        let _ = suffix;
        Ok(Self(value.to_owned()))
    }

    fn input(&self) -> Vec<u8> {
        let mut input = self.0.as_bytes().to_vec();
        input.push(b'\n');
        input
    }

    fn upload_input(&self, bytes: &[u8]) -> Vec<u8> {
        let mut input = self.input();
        input.extend_from_slice(bytes);
        input
    }

    fn activation_input(&self, reason: RemoteInstallReason, prior: &RemoteDaemonStatus) -> Vec<u8> {
        let mut input = self.input();
        match reason {
            RemoteInstallReason::Missing => input.extend_from_slice(b"missing\nabsent\n\n\n\n\n"),
            RemoteInstallReason::Upgrade => {
                input.extend_from_slice(b"upgrade\n");
                match prior {
                    RemoteDaemonStatus::Present {
                        protocol_version,
                        pid: Some(pid),
                        executable: Some(executable),
                        socket_device: Some(socket_device),
                        socket_inode: Some(socket_inode),
                    } => {
                        input.extend_from_slice(protocol_version.to_string().as_bytes());
                        input.push(b'\n');
                        input.extend_from_slice(pid.to_string().as_bytes());
                        input.push(b'\n');
                        input.extend_from_slice(executable.as_str().as_bytes());
                        input.push(b'\n');
                        input.extend_from_slice(socket_device.to_string().as_bytes());
                        input.push(b'\n');
                        input.extend_from_slice(socket_inode.to_string().as_bytes());
                        input.push(b'\n');
                    }
                    _ => input.extend_from_slice(b"absent\n\n\n\n\n"),
                }
            }
        }
        input
    }

    fn renewal_input(&self, renewal: u64) -> Vec<u8> {
        let mut input = self.input();
        input.extend_from_slice(renewal.to_string().as_bytes());
        input.push(b'\n');
        input
    }
}

fn parse_install_activation(output: &[u8]) -> io::Result<bool> {
    let fields = parse_nul_fields(output, INSTALL_ACTIVATION_PREFIX)?;
    match fields.as_slice() {
        [b"activated"] => Ok(true),
        [b"daemon_present"] => Ok(false),
        _ => Err(invalid_probe(
            "remote install activation returned an invalid result",
        )),
    }
}

fn shadow_upgrade_required(helper: &RemoteExecutable) -> io::Error {
    let path = helper.as_str();
    classified_error(
        io::ErrorKind::Unsupported,
        "upgrade_required",
        format!(
            "could not prove that the running remote daemon executable is the install destination; update the verified helper {path} through its owner or package mechanism, or explicitly stop the daemon before retrying"
        ),
    )
}

fn install_presence_required(message: &str) -> io::Error {
    classified_error(io::ErrorKind::Unsupported, "install_required", message)
}

fn upgrade_recovery_active() -> io::Error {
    classified_error(
        io::ErrorKind::WouldBlock,
        "busy",
        "a remote Boomux upgrade transaction is still recovering; wait for watchdog cleanup or inspect the owning Node before retrying",
    )
}

pub(crate) fn stale_upgrade_recovery() -> io::Error {
    classified_error(
        io::ErrorKind::WouldBlock,
        "upgrade_recovery_required",
        "a stale remote Boomux upgrade transaction is hiding the installed helper; inspect and recover that exact transaction before retrying",
    )
}

fn remote_recovery_error(state: RemoteRecoveryState) -> Option<io::Error> {
    match state {
        RemoteRecoveryState::Clear => None,
        RemoteRecoveryState::Active => Some(upgrade_recovery_active()),
        RemoteRecoveryState::Stale => Some(stale_upgrade_recovery()),
    }
}

fn parse_install_commit(output: &[u8]) -> io::Result<()> {
    let fields = parse_nul_fields(output, INSTALL_COMMIT_PREFIX)?;
    if fields.len() == 1 && fields[0] == b"committed" {
        Ok(())
    } else {
        Err(invalid_probe(
            "remote install commit returned an invalid result",
        ))
    }
}

pub struct RemoteConnection {
    child: Child,
    pid: i32,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr_reader: Option<BoundedReader>,
    pub executable: RemoteExecutable,
    pub handshake: FederationHandshake,
    _bootstrap_session: Option<BootstrapSession>,
}

pub(crate) struct RemoteAttachmentReader {
    child: Child,
    pid: i32,
    stdout: ChildStdout,
    stderr_reader: Option<BoundedReader>,
    _bootstrap_session: Option<BootstrapSession>,
}

pub(crate) struct RemoteAttachmentWriter(Option<ChildStdin>);

impl RemoteConnection {
    pub(crate) fn open_attachment(
        mut self,
        request: Request,
        timeout: Duration,
    ) -> io::Result<(Response, RemoteAttachmentReader, RemoteAttachmentWriter)> {
        let response = self.request(request, timeout)?;
        let mut this = std::mem::ManuallyDrop::new(self);
        // Ownership moves to the attachment while suppressing RemoteConnection's
        // process-group cleanup for the still-live channel.
        let (reader, writer) = unsafe {
            (
                RemoteAttachmentReader {
                    child: std::ptr::read(&this.child),
                    pid: this.pid,
                    stdout: std::ptr::read(&this.stdout),
                    stderr_reader: this.stderr_reader.take(),
                    _bootstrap_session: this._bootstrap_session.take(),
                },
                RemoteAttachmentWriter(this.stdin.take()),
            )
        };
        Ok((response, reader, writer))
    }
    pub(crate) fn request(&mut self, request: Request, timeout: Duration) -> io::Result<Response> {
        let version = self.handshake.core_protocol_version;
        protocol::write_message(
            self.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "remote channel closed")
            })?,
            &Envelope::with_version(version, request),
        )?;
        let response: Envelope<Response> = read_message_with_deadline(&mut self.stdout, timeout)?;
        if response.version != version {
            return Err(invalid_probe("remote response version mismatch"));
        }
        Ok(response.message)
    }

    pub fn ping(&mut self) -> io::Result<()> {
        let version = self.handshake.core_protocol_version;
        protocol::write_message(
            self.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "remote channel closed")
            })?,
            &Envelope::with_version(version, Request::Ping),
        )?;
        let response: Envelope<Response> = protocol::read_message(&mut self.stdout)?;
        if response.version == version && response.message == Response::Pong {
            Ok(())
        } else {
            Err(invalid_probe(
                "remote helper returned an invalid ping response",
            ))
        }
    }

    fn ping_with_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        match self.request(Request::Ping, timeout)? {
            Response::Pong => Ok(()),
            _ => Err(invalid_probe(
                "remote helper returned an invalid post-install ping response",
            )),
        }
    }

    pub(crate) fn node_projection_sync(
        &mut self,
        after: Option<protocol::EventCursor>,
        timeout: Duration,
    ) -> io::Result<protocol::NodeProjectionSync> {
        let version = self.handshake.core_protocol_version;
        if !protocol::ProtocolFeature::NodeProjectionSync.is_supported_by(version) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote Node does not support projection synchronization",
            ));
        }
        let wait_ms = if after.is_some() { 1_000 } else { 0 };
        protocol::write_message(
            self.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "remote channel closed")
            })?,
            &Envelope::with_version(version, Request::SyncNodeProjection { after, wait_ms }),
        )?;
        let response: Envelope<Response> = read_message_with_deadline(&mut self.stdout, timeout)?;
        if response.version != version {
            return Err(invalid_probe("remote projection response version mismatch"));
        }
        match response.message {
            Response::NodeProjectionSync { sync } => Ok(sync),
            Response::Error { message, .. } => Err(io::Error::other(message)),
            _ => Err(invalid_probe(
                "remote helper returned an invalid projection response",
            )),
        }
    }
}

impl RemoteAttachmentReader {
    pub(crate) fn read_frame(&mut self) -> io::Result<AttachFrame> {
        AttachFrame::read_from(&mut self.stdout)
    }

    pub(crate) fn close(&mut self) {
        let _ = kill_process_group(self.pid, &mut self.child);
    }
}

impl RemoteAttachmentWriter {
    pub(crate) fn write_frame(&mut self, frame: &AttachFrame, timeout: Duration) -> io::Result<()> {
        if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "attachment write deadline is outside the supported bound",
            ));
        }
        let stdin = self
            .0
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "remote channel closed"))?;
        let mut bytes = Vec::new();
        frame.write_to(&mut bytes)?;
        write_all_with_deadline(stdin, &bytes, timeout)
    }
}

fn write_all_with_deadline(
    writer: &mut (impl Write + AsRawFd),
    mut bytes: &[u8],
    timeout: Duration,
) -> io::Result<()> {
    let fd = writer.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let deadline = Instant::now() + timeout;
    let result = (|| {
        while !bytes.is_empty() {
            match writer.write(bytes) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "remote channel closed",
                    ));
                }
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "remote attachment write timed out",
                        ));
                    }
                    let mut descriptor = libc::pollfd {
                        fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    let milliseconds = i32::try_from(remaining.as_millis().min(i32::MAX as u128))
                        .unwrap_or(i32::MAX);
                    if unsafe { libc::poll(&mut descriptor, 1, milliseconds) } == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "remote attachment write timed out",
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        writer.flush()
    })();
    let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    result
}

impl Drop for RemoteAttachmentReader {
    fn drop(&mut self) {
        self.close();
        if let Some(reader) = self.stderr_reader.take() {
            let _ = join_bounded_reader(reader);
        }
    }
}

fn read_message_with_deadline<T: serde::de::DeserializeOwned>(
    reader: &mut (impl Read + AsRawFd),
    timeout: Duration,
) -> io::Result<T> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "response deadline is outside the supported bound",
        ));
    }
    let fd = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let deadline = Instant::now() + timeout;
    let result = (|| {
        let mut length = [0_u8; 4];
        read_exact_with_deadline(reader, fd, &mut length, deadline)?;
        let length = u32::from_be_bytes(length) as usize;
        if length > protocol::MAX_CONTROL_FRAME {
            return Err(invalid_probe("remote control frame exceeds the size limit"));
        }
        let mut bytes = vec![0; length];
        read_exact_with_deadline(reader, fd, &mut bytes, deadline)?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    })();
    let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    result
}

fn read_exact_with_deadline(
    reader: &mut impl Read,
    fd: i32,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        match reader.read(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "remote channel closed",
                ));
            }
            Ok(count) => bytes = &mut bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "remote response timed out",
                    ));
                }
                let mut descriptor = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let milliseconds =
                    i32::try_from(remaining.as_millis().min(i32::MAX as u128)).unwrap_or(i32::MAX);
                let status = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
                if status == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "remote response timed out",
                    ));
                }
                if status == -1 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

impl Drop for RemoteConnection {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = kill_process_group(self.pid, &mut self.child);
        if let Some(reader) = self.stderr_reader.take() {
            let _ = join_bounded_reader(reader);
        }
    }
}

impl RemotePlatform {
    pub fn parse_probe(output: &[u8]) -> io::Result<Self> {
        let fields = parse_nul_fields(output, PLATFORM_PROBE_PREFIX)?;
        if fields.len() != 2 {
            return Err(invalid_probe(
                "platform probe returned an invalid field count",
            ));
        }
        let operating_system = match fields[0] {
            b"Linux" => RemoteOperatingSystem::Linux,
            b"Darwin" => RemoteOperatingSystem::MacOs,
            _ => {
                return Err(classified_error(
                    io::ErrorKind::Unsupported,
                    "bootstrap_unsupported_platform",
                    "remote operating system is unsupported",
                ));
            }
        };
        let architecture = match fields[1] {
            b"x86_64" | b"amd64" => RemoteArchitecture::X86_64,
            b"aarch64" | b"arm64" => RemoteArchitecture::Aarch64,
            _ => {
                return Err(classified_error(
                    io::ErrorKind::Unsupported,
                    "bootstrap_unsupported_platform",
                    "remote architecture is unsupported",
                ));
            }
        };
        Ok(Self {
            operating_system,
            architecture,
        })
    }

    pub const fn release_target(self) -> Option<&'static str> {
        match (self.operating_system, self.architecture) {
            (RemoteOperatingSystem::Linux, RemoteArchitecture::X86_64) => {
                Some("x86_64-unknown-linux-gnu")
            }
            _ => None,
        }
    }

    fn matches_local(self) -> bool {
        matches!(
            (
                self.operating_system,
                self.architecture,
                env::consts::OS,
                env::consts::ARCH
            ),
            (
                RemoteOperatingSystem::Linux,
                RemoteArchitecture::X86_64,
                "linux",
                "x86_64"
            ) | (
                RemoteOperatingSystem::Linux,
                RemoteArchitecture::Aarch64,
                "linux",
                "aarch64"
            ) | (
                RemoteOperatingSystem::MacOs,
                RemoteArchitecture::X86_64,
                "macos",
                "x86_64"
            ) | (
                RemoteOperatingSystem::MacOs,
                RemoteArchitecture::Aarch64,
                "macos",
                "aarch64"
            )
        )
    }
}

pub fn parse_executable_probe(output: &[u8]) -> io::Result<Vec<RemoteExecutable>> {
    let fields = parse_nul_fields(output, EXECUTABLE_PROBE_PREFIX)?;
    if fields.len() > MAX_DISCOVERED_EXECUTABLES {
        return Err(invalid_probe(
            "executable probe returned too many candidates",
        ));
    }
    let mut seen = HashSet::new();
    let mut executables = Vec::new();
    for field in fields {
        let value = std::str::from_utf8(field)
            .map_err(|_| invalid_probe("executable probe returned non-UTF-8 data"))?;
        let executable = RemoteExecutable::parse(value.to_owned())
            .map_err(|_| invalid_probe("executable probe returned an invalid executable path"))?;
        if seen.insert(executable.0.clone()) {
            executables.push(executable);
        }
    }
    Ok(executables)
}

pub fn parse_install_destination_probe(output: &[u8]) -> io::Result<RemoteExecutable> {
    parse_install_destination_state(output).map(|(destination, _)| destination)
}

fn parse_install_destination_state(
    output: &[u8],
) -> io::Result<(RemoteExecutable, RemoteRecoveryState)> {
    let fields = parse_nul_fields(output, INSTALL_DESTINATION_PROBE_PREFIX)?;
    let recovery = match fields.as_slice() {
        [_] | [_, b"clear"] => RemoteRecoveryState::Clear,
        [_, b"recovering"] => RemoteRecoveryState::Active,
        [_, b"stale"] => RemoteRecoveryState::Stale,
        _ => {
            return Err(invalid_probe(
                "install destination probe returned an invalid field count or state",
            ));
        }
    };
    let value = std::str::from_utf8(fields[0])
        .map_err(|_| invalid_probe("install destination probe returned non-UTF-8 data"))?;
    let destination = RemoteExecutable::parse(value.to_owned()).map_err(|_| {
        invalid_probe("install destination probe returned an invalid executable path")
    })?;
    Ok((destination, recovery))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget(String);

impl SshTarget {
    pub fn parse(target: impl Into<String>) -> io::Result<Self> {
        let target = target.into();
        if target.is_empty()
            || target.len() > MAX_SSH_TARGET_BYTES
            || target.starts_with('-')
            || target.chars().any(char::is_whitespace)
            || target.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SSH target must be a bounded non-option argument without whitespace",
            ));
        }
        Ok(Self(target))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteExecutable(String);

impl RemoteExecutable {
    pub fn parse(path: impl Into<String>) -> io::Result<Self> {
        let path = path.into();
        if path.is_empty()
            || path.len() > MAX_REMOTE_EXECUTABLE_BYTES
            || !Path::new(&path).is_absolute()
            || path.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote Boomux executable must be a bounded absolute path",
            ));
        }
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub struct SshInvocation {
    program: OsString,
    directory: PathBuf,
    config_path: PathBuf,
    control_path: PathBuf,
    target: SshTarget,
    remote_command: String,
    authentication: SshAuthenticationMode,
}

pub struct BootstrapSession {
    id: Uuid,
    program: OsString,
    directory: PathBuf,
    config_path: PathBuf,
    control_path: PathBuf,
    target: SshTarget,
    master: Child,
    master_pid: i32,
    stderr_reader: Option<MasterStderrReader>,
}

struct TerminalForegroundGuard {
    terminal: fs::File,
    original_group: i32,
}

impl TerminalForegroundGuard {
    fn acquire(terminal: fs::File, group: i32) -> io::Result<Self> {
        let original_group = unsafe { libc::tcgetpgrp(terminal.as_raw_fd()) };
        if original_group == -1 {
            return Err(io::Error::last_os_error());
        }
        set_terminal_foreground(terminal.as_raw_fd(), group)?;
        Ok(Self {
            terminal,
            original_group,
        })
    }
}

impl Drop for TerminalForegroundGuard {
    fn drop(&mut self) {
        let _ = set_terminal_foreground(self.terminal.as_raw_fd(), self.original_group);
    }
}

fn set_terminal_foreground(fd: i32, group: i32) -> io::Result<()> {
    unsafe {
        let previous = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        let result = libc::tcsetpgrp(fd, group);
        libc::signal(libc::SIGTTOU, previous);
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshProbeOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

type BoundedReader = thread::JoinHandle<io::Result<(Vec<u8>, bool)>>;

trait StderrMirror: Write + AsRawFd + Send {}

impl<T: Write + AsRawFd + Send> StderrMirror for T {}

type InteractiveTerminal = (Option<Box<dyn StderrMirror>>, Option<fs::File>);

#[derive(Debug, Clone, Copy)]
enum MasterStderrEvent {
    Truncated,
    MirrorFailed,
}

struct MasterStderrReader {
    reader: BoundedReader,
    events: mpsc::Receiver<MasterStderrEvent>,
}

impl MasterStderrReader {
    fn event(&self) -> Option<MasterStderrEvent> {
        self.events.try_recv().ok()
    }

    fn finish(self) -> (io::Result<(Vec<u8>, bool)>, Option<MasterStderrEvent>) {
        let result = join_bounded_reader(self.reader);
        (result, self.events.try_recv().ok())
    }
}

impl BootstrapSession {
    pub fn open(
        target: SshTarget,
        authentication: SshAuthenticationMode,
        timeout: Duration,
    ) -> io::Result<Self> {
        let socket_path = crate::client::socket_path()?;
        let runtime_directory = socket_path
            .parent()
            .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?;
        let user_config = env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .map(|home| home.join(".ssh/config"));
        let (stderr_mirror, foreground_terminal) = match authentication {
            SshAuthenticationMode::Interactive => (
                Some(Box::new(
                    OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
                        .open("/dev/tty")?,
                ) as Box<dyn StderrMirror>),
                Some(
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(libc::O_CLOEXEC)
                        .open("/dev/tty")?,
                ),
            ),
            SshAuthenticationMode::Batch => (None, None),
        };
        Self::open_at_with_mirror(
            runtime_directory,
            user_config.as_deref(),
            target,
            authentication,
            timeout,
            OsStr::new("ssh"),
            (stderr_mirror, foreground_terminal),
        )
    }

    #[cfg(test)]
    fn open_at(
        runtime_directory: &Path,
        user_config: Option<&Path>,
        target: SshTarget,
        authentication: SshAuthenticationMode,
        timeout: Duration,
        program: &OsStr,
    ) -> io::Result<Self> {
        Self::open_at_with_mirror(
            runtime_directory,
            user_config,
            target,
            authentication,
            timeout,
            program,
            (None, None),
        )
    }

    fn open_at_with_mirror(
        runtime_directory: &Path,
        user_config: Option<&Path>,
        target: SshTarget,
        authentication: SshAuthenticationMode,
        timeout: Duration,
        program: &OsStr,
        interactive_terminal: InteractiveTerminal,
    ) -> io::Result<Self> {
        let (stderr_mirror, foreground_terminal) = interactive_terminal;
        if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SSH bootstrap timeout is outside the supported bound",
            ));
        }
        let (directory, config_path, control_path) =
            prepare_ssh_directory(runtime_directory, user_config)?;
        let mut command = Command::new(program);
        append_ssh_security_options(&mut command, &config_path, &control_path, authentication);
        command
            .args(["-o", "ControlMaster=yes"])
            .args(["-o", "ControlPersist=no"])
            .arg("-N")
            .arg(target.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(move || {
                let result = match authentication {
                    SshAuthenticationMode::Interactive => libc::setpgid(0, 0),
                    SshAuthenticationMode::Batch => libc::setsid(),
                };
                if result == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut master = match command.spawn() {
            Ok(master) => master,
            Err(error) => {
                let _ = fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        let master_pid = i32::try_from(master.id()).map_err(|error| {
            let _ = master.kill();
            let _ = master.wait();
            let _ = fs::remove_dir_all(&directory);
            io::Error::other(format!("child PID overflow: {error}"))
        })?;
        let _foreground = match foreground_terminal {
            Some(terminal) => match TerminalForegroundGuard::acquire(terminal, master_pid) {
                Ok(foreground) => Some(foreground),
                Err(error) => {
                    let _ = kill_process_group(master_pid, &mut master);
                    let _ = fs::remove_dir_all(&directory);
                    return Err(error);
                }
            },
            None => None,
        };
        let stderr = match master.stderr.take() {
            Some(stderr) => stderr,
            None => {
                let _ = kill_process_group(master_pid, &mut master);
                let _ = fs::remove_dir_all(&directory);
                return Err(io::Error::other("SSH master stderr was not captured"));
            }
        };
        let deadline = Instant::now() + timeout;
        let stderr_reader = match spawn_master_stderr_reader(
            stderr,
            MAX_PROBE_STDERR_BYTES,
            if authentication == SshAuthenticationMode::Interactive {
                stderr_mirror
            } else {
                None
            },
            master_pid,
            deadline,
        ) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = kill_process_group(master_pid, &mut master);
                let _ = fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        loop {
            if let Some(event) = stderr_reader.event() {
                let _ = kill_process_group(master_pid, &mut master);
                let _ = stderr_reader.finish();
                let _ = fs::remove_dir_all(&directory);
                return Err(master_stderr_event_error(event));
            }
            let status = match master.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    let _ = kill_process_group(master_pid, &mut master);
                    let _ = stderr_reader.finish();
                    let _ = fs::remove_dir_all(&directory);
                    return Err(error);
                }
            };
            if let Some(status) = status {
                let (result, event) = stderr_reader.finish();
                let _ = fs::remove_dir_all(&directory);
                if let Some(event) = event {
                    return Err(master_stderr_event_error(event));
                }
                let (stderr, truncated) = result?;
                if truncated {
                    return Err(master_stderr_event_error(MasterStderrEvent::Truncated));
                }
                return Err(classify_ssh_start_failure(status.code(), &stderr));
            }
            let mut check = Command::new(program);
            check
                .arg("-F")
                .arg(&config_path)
                .arg("-S")
                .arg(&control_path)
                .args(["-O", "check"])
                .arg(target.as_str())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if check.status().is_ok_and(|status| status.success()) {
                if let Some(event) = stderr_reader.event() {
                    let _ = kill_process_group(master_pid, &mut master);
                    let _ = stderr_reader.finish();
                    let _ = fs::remove_dir_all(&directory);
                    return Err(master_stderr_event_error(event));
                }
                break;
            }
            if Instant::now() >= deadline {
                let _ = kill_process_group(master_pid, &mut master);
                let _ = stderr_reader.finish();
                let _ = fs::remove_dir_all(&directory);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "SSH bootstrap authentication timed out",
                ));
            }
            thread::sleep(CHILD_POLL_INTERVAL);
        }
        Ok(Self {
            id: Uuid::new_v4(),
            program: program.to_owned(),
            directory,
            config_path,
            control_path,
            target,
            master,
            master_pid,
            stderr_reader: Some(stderr_reader),
        })
    }

    fn command(&self, remote_command: &str) -> Command {
        slave_command(
            &self.program,
            &self.config_path,
            &self.control_path,
            &self.target,
            remote_command,
        )
    }

    pub fn plan(&mut self, timeout: Duration) -> io::Result<RemoteBootstrapPlan> {
        let discovery = discover_remote_in_session(self, timeout)?;
        let selection = inspect_remote_helpers_in_session(self, &discovery.executables, timeout)?;
        if let Some(helper) = selection.compatible {
            return Ok(RemoteBootstrapPlan::Ready(helper));
        }
        if let Some(error) = remote_recovery_error(discovery.recovery) {
            return Err(error);
        }
        let reason = if discovery.executables.is_empty() {
            RemoteInstallReason::Missing
        } else if selection.incompatible == discovery.executables.len() {
            RemoteInstallReason::Upgrade
        } else {
            return Err(io::Error::other(
                "remote Boomux candidates could not be verified; refusing remote modification",
            ));
        };
        let source = select_install_source(discovery.platform)?;
        let upgrade_helper = (reason == RemoteInstallReason::Upgrade)
            .then(|| selection.incompatible_executables.first().cloned())
            .flatten();
        Ok(RemoteBootstrapPlan::Install(RemoteInstallPlan {
            target: self.target.clone(),
            destination: discovery.install_destination,
            source,
            reason,
            bootstrap_id: Some(self.id),
            upgrade_helper,
            intent: RemoteInstallIntent::AutomaticCompatibility,
        }))
    }

    pub fn plan_explicit_upgrade(
        &mut self,
        expected_node_id: &str,
        timeout: Duration,
    ) -> io::Result<RemoteInstallPlan> {
        let discovery = discover_remote_in_session(self, timeout)?;
        let selection = inspect_remote_helpers_in_session(self, &discovery.executables, timeout)?;
        let helper = selection.compatible.ok_or_else(|| {
            if let Some(error) = remote_recovery_error(discovery.recovery) {
                return error;
            }
            classified_error(
                io::ErrorKind::Unsupported,
                "upgrade_required",
                "explicit registered Node upgrade requires a currently compatible remote helper",
            )
        })?;
        let intent = RemoteInstallIntent::ExplicitRegisteredUpgrade {
            expected_node_id: expected_node_id.to_owned(),
        };
        intent.verify_helper_identity(&helper)?;
        let source = select_install_source(discovery.platform)?;
        Ok(RemoteInstallPlan {
            target: self.target.clone(),
            destination: discovery.install_destination,
            source,
            reason: RemoteInstallReason::Upgrade,
            bootstrap_id: Some(self.id),
            upgrade_helper: Some(helper.executable),
            intent,
        })
    }

    pub fn connect_existing_verified(
        self,
        expected_node_id: &str,
        timeout: Duration,
    ) -> io::Result<RemoteConnection> {
        let discovery = discover_remote_in_session(&self, timeout)?;
        let selection = inspect_remote_helpers_in_session(&self, &discovery.executables, timeout)?;
        let helper = selection.compatible.ok_or_else(|| {
            if let Some(error) = remote_recovery_error(discovery.recovery) {
                return error;
            }
            let (code, message) = if discovery.executables.is_empty() {
                (
                    "install_required",
                    "registered Node reauthentication requires an existing compatible remote helper",
                )
            } else {
                (
                    "upgrade_required",
                    "registered Node reauthentication found no compatible remote helper",
                )
            };
            classified_error(io::ErrorKind::Unsupported, code, message)
        })?;
        RemoteInstallIntent::ExplicitRegisteredUpgrade {
            expected_node_id: expected_node_id.to_owned(),
        }
        .verify_helper_identity(&helper)?;
        let mut connection = self.connect(helper, timeout)?;
        connection.ping_with_timeout(timeout)?;
        Ok(connection)
    }

    pub fn connect(
        self,
        helper: CompatibleRemoteHelper,
        timeout: Duration,
    ) -> io::Result<RemoteConnection> {
        if helper.bootstrap_id != Some(self.id) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote helper observation belongs to a different bootstrap endpoint",
            ));
        }
        connect_remote_in_session(self, helper, timeout)
    }

    pub fn install_and_connect(
        self,
        plan: &RemoteInstallPlan,
        timeout: Duration,
    ) -> io::Result<RemoteConnection> {
        self.install_and_connect_guarded(plan, timeout, || Ok(()))
    }

    pub fn install_and_connect_guarded(
        self,
        plan: &RemoteInstallPlan,
        timeout: Duration,
        mut maintenance: impl FnMut() -> io::Result<()>,
    ) -> io::Result<RemoteConnection> {
        maintenance().map_err(|error| {
            with_recovery(error, BootstrapRecoveryDisposition::NoRemoteMutation)
        })?;
        if plan.bootstrap_id != Some(self.id) || plan.target != self.target {
            return Err(classified_error(
                io::ErrorKind::PermissionDenied,
                "bootstrap_authentication_failed",
                "remote install authorization belongs to a different bootstrap endpoint",
            ));
        }
        let upgrade_helper = if plan.reason == RemoteInstallReason::Upgrade {
            Some(plan.upgrade_helper.as_ref().ok_or_else(|| {
                classified_error(
                    io::ErrorKind::InvalidData,
                    "bootstrap_malformed_helper",
                    "remote upgrade omitted its verified outdated helper",
                )
            })?)
        } else {
            prove_remote_daemon_absent(&self, timeout).map_err(|error| {
                with_recovery(error, BootstrapRecoveryDisposition::NoRemoteMutation)
            })?;
            None
        };
        let requested_transaction = InstallTransactionId::generate();
        let upload = || {
            run_streaming_command_capture(
                self.command(REMOTE_INSTALL_COMMAND),
                requested_transaction.upload_input(plan.source.bytes()),
                timeout,
            )
            .and_then(|output| InstallTransactionId::parse_probe(&output.stdout))
        };
        let transaction = match upload() {
            Ok(transaction) if transaction == requested_transaction => transaction,
            Ok(_) => {
                return Err(post_install_failure(
                    "stream",
                    invalid_probe("remote upload acknowledged a different transaction"),
                ));
            }
            Err(first_error) => {
                if let Err(error) = maintenance() {
                    let failure = post_install_failure("stream", error);
                    return Err(
                        match self.rollback_install(&requested_transaction, timeout) {
                            Ok(()) => with_recovery(
                                failure,
                                BootstrapRecoveryDisposition::RollbackConfirmed,
                            ),
                            Err(_) => failure,
                        },
                    );
                }
                match upload() {
                    Ok(transaction) if transaction == requested_transaction => transaction,
                    Ok(_) => {
                        return Err(post_install_failure(
                            "stream",
                            invalid_probe("upload retry acknowledged a different transaction"),
                        ));
                    }
                    Err(retry_error) => {
                        return Err(post_install_failure(
                            "stream",
                            io::Error::new(
                                retry_error.kind(),
                                format!(
                                    "upload retry failed after ambiguous outcome: {first_error}"
                                ),
                            ),
                        ));
                    }
                }
            }
        };
        let mut renewal = 0_u64;
        macro_rules! renew_lease {
            ($stage:literal) => {{
                renewal += 1;
                if let Err(error) = maintenance() {
                    let failure = post_install_failure($stage, error);
                    return Err(match self.rollback_install(&transaction, timeout) {
                        Ok(()) => {
                            with_recovery(failure, BootstrapRecoveryDisposition::RollbackConfirmed)
                        }
                        Err(_) => failure,
                    });
                }
                if let Err(error) = run_streaming_command(
                    self.command(REMOTE_INSTALL_RENEW_COMMAND),
                    transaction.renewal_input(renewal),
                    timeout,
                ) {
                    let failure = post_install_failure($stage, error);
                    return Err(match self.rollback_install(&transaction, timeout) {
                        Ok(()) => {
                            with_recovery(failure, BootstrapRecoveryDisposition::RollbackConfirmed)
                        }
                        Err(_) => failure,
                    });
                }
            }};
        }
        renew_lease!("daemon_status");
        let prior_daemon = if let Some(helper) = upgrade_helper {
            let status = run_bounded_command(
                self.command(&remote_transaction_daemon_status_command(&transaction)),
                timeout,
            )
            .and_then(|output| parse_remote_daemon_status(&output.stdout));
            match status {
                Ok(status) if status.proves_executable(&plan.destination) => status,
                _ => {
                    let failure = shadow_upgrade_required(helper);
                    return Err(match self.rollback_install(&transaction, timeout) {
                        Ok(()) => {
                            with_recovery(failure, BootstrapRecoveryDisposition::RollbackConfirmed)
                        }
                        Err(_) => {
                            with_recovery(failure, BootstrapRecoveryDisposition::OutcomeUnknown)
                        }
                    });
                }
            }
        } else {
            RemoteDaemonStatus::Absent
        };
        renew_lease!("activation");
        let activate = || {
            run_streaming_command_capture(
                self.command(REMOTE_INSTALL_ACTIVATE_COMMAND),
                transaction.activation_input(plan.reason, &prior_daemon),
                timeout,
            )
            .and_then(|output| parse_install_activation(&output.stdout))
        };
        let activated = match activate() {
            Ok(activated) => activated,
            Err(first_error) => match activate() {
                Ok(activated) => activated,
                Err(retry_error) => {
                    return Err(post_install_failure(
                        "activation",
                        io::Error::new(
                            retry_error.kind(),
                            format!(
                                "activation retry failed after ambiguous outcome: {first_error}"
                            ),
                        ),
                    ));
                }
            },
        };
        if !activated {
            let failure = install_presence_required(
                "a remote daemon socket appeared before activation; recover or stop that daemon before retrying installation",
            );
            return Err(match self.rollback_install(&transaction, timeout) {
                Ok(()) => with_recovery(failure, BootstrapRecoveryDisposition::RollbackConfirmed),
                Err(_) => with_recovery(failure, BootstrapRecoveryDisposition::OutcomeUnknown),
            });
        }
        renew_lease!("helper_verification");
        if let Err(error) = run_streaming_command(
            self.command(REMOTE_INSTALL_MARK_DAEMON_CONTACTED_COMMAND),
            transaction.input(),
            timeout,
        ) {
            self.rollback_install(&transaction, timeout)?;
            return Err(post_contact_failure(
                "helper_verification",
                error,
                plan.reason,
            ));
        }
        renew_lease!("helper_verification");
        let initial_helper = (|| {
            let candidates = [plan.destination.clone()];
            let helper =
                match inspect_remote_helpers_in_session(&self, &candidates, timeout)?.compatible {
                    Some(helper) => helper,
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "installed remote helper is not federation-compatible",
                        ));
                    }
                };
            plan.intent.verify_helper_identity(&helper)?;
            Ok(helper)
        })();
        if let (RemoteInstallIntent::ExplicitRegisteredUpgrade { .. }, Err(error)) =
            (&plan.intent, &initial_helper)
        {
            let error = io::Error::new(error.kind(), error.to_string());
            self.rollback_install(&transaction, timeout)?;
            return Err(post_contact_failure(
                "helper_verification",
                error,
                plan.reason,
            ));
        }
        let daemon_status = if plan.reason == RemoteInstallReason::Upgrade {
            renew_lease!("daemon_status");
            match remote_daemon_status_in_session(&self, &plan.destination, timeout) {
                Ok(status) => Some(status),
                Err(error) => {
                    self.rollback_install(&transaction, timeout)?;
                    return Err(post_contact_failure("daemon_status", error, plan.reason));
                }
            }
        } else {
            None
        };
        let helper = if daemon_status
            .as_ref()
            .is_some_and(|status| plan.intent.daemon_restart_required(status))
        {
            renew_lease!("daemon_restart");
            if let Err(error) = run_streaming_command(
                self.command(REMOTE_INSTALL_MARK_RESTARTED_COMMAND),
                transaction.input(),
                timeout,
            ) {
                self.rollback_install(&transaction, timeout)?;
                return Err(post_contact_failure("daemon_restart", error, plan.reason));
            }
            renew_lease!("daemon_restart");
            if let Err(error) = run_bounded_command(
                self.command(&remote_installed_daemon_command("daemon restart")),
                timeout,
            ) {
                self.rollback_install(&transaction, timeout)?;
                return Err(post_contact_failure("daemon_restart", error, plan.reason));
            }
            renew_lease!("helper_verification");
            match inspect_remote_helpers_in_session(
                &self,
                std::slice::from_ref(&plan.destination),
                timeout,
            )
            .and_then(|inspection| {
                let helper = inspection.compatible.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "installed remote helper failed after daemon restart",
                    )
                })?;
                plan.intent.verify_helper_identity(&helper)?;
                Ok(helper)
            }) {
                Ok(helper) => helper,
                Err(error) => {
                    self.rollback_install(&transaction, timeout)?;
                    return Err(post_contact_failure(
                        "helper_verification",
                        error,
                        plan.reason,
                    ));
                }
            }
        } else {
            match initial_helper {
                Ok(helper) => helper,
                Err(error) => {
                    self.rollback_install(&transaction, timeout)?;
                    return Err(post_contact_failure(
                        "helper_verification",
                        error,
                        plan.reason,
                    ));
                }
            }
        };

        renew_lease!("live_handshake");
        let command = self.command(&remote_helper_command(&helper.executable));
        let mut connection = match connect_remote_command(command, helper, timeout, None) {
            Ok(connection) => connection,
            Err(error) => {
                self.rollback_install(&transaction, timeout)?;
                return Err(post_contact_failure("live_handshake", error, plan.reason));
            }
        };
        renew_lease!("protocol_ping");
        if let Err(error) = connection.ping_with_timeout(timeout) {
            drop(connection);
            self.rollback_install(&transaction, timeout)?;
            return Err(post_contact_failure("protocol_ping", error, plan.reason));
        }
        renew_lease!("commit");
        let commit = run_streaming_command_capture(
            self.command(REMOTE_INSTALL_COMMIT_COMMAND),
            transaction.input(),
            timeout,
        )
        .and_then(|output| parse_install_commit(&output.stdout));
        if let Err(error) = commit {
            drop(connection);
            return Err(classified_error_with_recovery(
                io::ErrorKind::Other,
                "bootstrap_commit_outcome_unknown",
                format!(
                    "remote install commit outcome is unknown after successful helper verification and protocol ping; retry the exact bootstrap to discover the installed helper: {error}"
                ),
                BootstrapRecoveryDisposition::OutcomeUnknown,
            ));
        }
        connection._bootstrap_session = Some(self);
        Ok(connection)
    }

    fn rollback_install(
        &self,
        transaction: &InstallTransactionId,
        timeout: Duration,
    ) -> io::Result<()> {
        run_streaming_command(
            self.command(REMOTE_INSTALL_ROLLBACK_COMMAND),
            transaction.input(),
            timeout,
        )
        .map_err(|error| with_recovery(error, BootstrapRecoveryDisposition::OutcomeUnknown))
    }
}

fn slave_command(
    program: &OsStr,
    config_path: &Path,
    control_path: &Path,
    target: &SshTarget,
    remote_command: &str,
) -> Command {
    let mut command = Command::new(program);
    append_ssh_security_options(
        &mut command,
        config_path,
        control_path,
        SshAuthenticationMode::Batch,
    );
    command
        .args(["-o", "ControlMaster=no"])
        .args(["-o", "HostName=boomux-mux-only.invalid"])
        .args(["-o", "ProxyCommand=/bin/false"])
        .args(["-o", "ConnectionAttempts=1"])
        .arg(target.as_str())
        .arg(remote_command);
    command
}

impl Drop for BootstrapSession {
    fn drop(&mut self) {
        let _ = kill_process_group(self.master_pid, &mut self.master);
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.finish();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

impl SshInvocation {
    pub fn prepare(
        target: SshTarget,
        executable: RemoteExecutable,
        authentication: SshAuthenticationMode,
    ) -> io::Result<Self> {
        let socket_path = crate::client::socket_path()?;
        let runtime_directory = socket_path
            .parent()
            .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?;
        let user_config = env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .map(|home| home.join(".ssh/config"));
        Self::prepare_at(
            runtime_directory,
            user_config.as_deref(),
            target,
            executable,
            authentication,
        )
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .arg("-F")
            .arg(&self.config_path)
            .arg("-T")
            .args(["-o", "ClearAllForwardings=yes"])
            .args(["-o", "ForwardAgent=no"])
            .args(["-o", "ForwardX11=no"])
            .args(["-o", "PermitLocalCommand=no"])
            .args(["-o", "RemoteCommand=none"])
            .args(["-o", "ForkAfterAuthentication=no"])
            .args(["-o", "StdinNull=no"])
            .args(["-o", "SessionType=default"])
            .args(["-o", "ControlMaster=auto"])
            .args(["-o", "ControlPersist=no"])
            .arg("-o")
            .arg(format!(
                "ControlPath={}",
                self.control_path
                    .to_str()
                    .expect("validated SSH control path")
            ))
            .args([
                "-o",
                match self.authentication {
                    SshAuthenticationMode::Interactive => "BatchMode=no",
                    SshAuthenticationMode::Batch => "BatchMode=yes",
                },
            ])
            .arg(self.target.as_str())
            .arg(&self.remote_command);
        command
    }

    pub fn prepare_probe(
        target: SshTarget,
        probe: RemoteProbe,
        authentication: SshAuthenticationMode,
    ) -> io::Result<Self> {
        let socket_path = crate::client::socket_path()?;
        let runtime_directory = socket_path
            .parent()
            .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?;
        let user_config = env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .map(|home| home.join(".ssh/config"));
        Self::prepare_command_at(
            runtime_directory,
            user_config.as_deref(),
            target,
            probe.command().to_owned(),
            authentication,
        )
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    pub fn run_probe(&self, timeout: Duration) -> io::Result<SshProbeOutput> {
        run_bounded_command(self.command(), timeout)
    }

    pub fn verify_helper(&self, timeout: Duration) -> io::Result<FederationHandshake> {
        run_helper_probe_command(self.command(), timeout)
    }

    fn prepare_at(
        runtime_directory: &Path,
        user_config: Option<&Path>,
        target: SshTarget,
        executable: RemoteExecutable,
        authentication: SshAuthenticationMode,
    ) -> io::Result<Self> {
        Self::prepare_command_at(
            runtime_directory,
            user_config,
            target,
            remote_helper_command(&executable),
            authentication,
        )
    }

    fn prepare_command_at(
        runtime_directory: &Path,
        user_config: Option<&Path>,
        target: SshTarget,
        remote_command: String,
        authentication: SshAuthenticationMode,
    ) -> io::Result<Self> {
        Self::prepare_command_with_program_at(
            runtime_directory,
            user_config,
            target,
            remote_command,
            authentication,
            OsStr::new("ssh"),
        )
    }

    fn prepare_command_with_program_at(
        runtime_directory: &Path,
        user_config: Option<&Path>,
        target: SshTarget,
        remote_command: String,
        authentication: SshAuthenticationMode,
        program: &OsStr,
    ) -> io::Result<Self> {
        secure_runtime_directory(runtime_directory)?;
        let nonce = Uuid::new_v4().simple().to_string();
        let directory = runtime_directory.join(format!("ssh-{}", &nonce[..16]));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let config_path = directory.join("config");
        let control_path = directory.join("c");
        if control_path.as_os_str().as_bytes().len() > MAX_CONTROL_PATH_BYTES {
            let _ = fs::remove_dir_all(&directory);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SSH control socket path exceeds the safe Unix socket bound",
            ));
        }
        validate_option_path(&control_path, "SSH control socket")?;
        let result = (|| {
            let mut config = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&config_path)?;
            if let Some(user_config) = user_config {
                writeln!(config, "Include {}", quote_ssh_config_path(user_config)?)?;
                writeln!(config, "Match all")?;
            }
            // SendEnv is list-valued, so clear user entries after their config.
            writeln!(config, "SendEnv -*")?;
            writeln!(config, "Host *")?;
            writeln!(config, "    ServerAliveInterval 15")?;
            writeln!(config, "    ServerAliveCountMax 3")?;
            config.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&directory);
            return Err(error);
        }
        Ok(Self {
            program: program.to_owned(),
            directory,
            config_path,
            control_path,
            target,
            remote_command,
            authentication,
        })
    }
}

fn append_ssh_security_options(
    command: &mut Command,
    config_path: &Path,
    control_path: &Path,
    authentication: SshAuthenticationMode,
) {
    command
        .arg("-F")
        .arg(config_path)
        .arg("-T")
        .args(["-o", "ClearAllForwardings=yes"])
        .args(["-o", "ForwardAgent=no"])
        .args(["-o", "ForwardX11=no"])
        .args(["-o", "PermitLocalCommand=no"])
        .args(["-o", "RemoteCommand=none"])
        .args(["-o", "ForkAfterAuthentication=no"])
        .args(["-o", "StdinNull=no"])
        .arg("-o")
        .arg(format!(
            "ControlPath={}",
            control_path.to_str().expect("validated SSH control path")
        ))
        .args([
            "-o",
            match authentication {
                SshAuthenticationMode::Interactive => "BatchMode=no",
                SshAuthenticationMode::Batch => "BatchMode=yes",
            },
        ]);
}

fn prepare_ssh_directory(
    runtime_directory: &Path,
    user_config: Option<&Path>,
) -> io::Result<(PathBuf, PathBuf, PathBuf)> {
    secure_runtime_directory(runtime_directory)?;
    let nonce = Uuid::new_v4().simple().to_string();
    let directory = runtime_directory.join(format!("ssh-{}", &nonce[..16]));
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let config_path = directory.join("config");
    let control_path = directory.join("c");
    if control_path.as_os_str().as_bytes().len() > MAX_CONTROL_PATH_BYTES {
        let _ = fs::remove_dir_all(&directory);
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH control socket path exceeds the safe Unix socket bound",
        ));
    }
    validate_option_path(&control_path, "SSH control socket")?;
    let result = (|| {
        let mut config = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&config_path)?;
        if let Some(user_config) = user_config {
            writeln!(config, "Include {}", quote_ssh_config_path(user_config)?)?;
            writeln!(config, "Match all")?;
        }
        writeln!(config, "SendEnv -*")?;
        writeln!(config, "Host *")?;
        writeln!(config, "    ServerAliveInterval 15")?;
        writeln!(config, "    ServerAliveCountMax 3")?;
        config.sync_all()
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&directory);
        return Err(error);
    }
    Ok((directory, config_path, control_path))
}

fn classify_ssh_start_failure(status: Option<i32>, stderr: &[u8]) -> io::Error {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if stderr.contains("permission denied")
        || stderr.contains("authentication failed")
        || stderr.contains("host key verification failed")
    {
        classified_error(
            io::ErrorKind::PermissionDenied,
            "bootstrap_authentication_failed",
            "SSH authentication or host-key verification failed",
        )
    } else {
        classified_error(
            io::ErrorKind::ConnectionRefused,
            "bootstrap_transport_failed",
            format!(
                "SSH bootstrap transport failed{}",
                status.map_or_else(String::new, |status| format!(" with status {status}"))
            ),
        )
    }
}

fn master_stderr_event_error(event: MasterStderrEvent) -> io::Error {
    let message = match event {
        MasterStderrEvent::Truncated => {
            "SSH bootstrap authentication output exceeded the supported bound"
        }
        MasterStderrEvent::MirrorFailed => {
            "SSH bootstrap authentication output could not be shown safely"
        }
    };
    classified_error(
        io::ErrorKind::ConnectionAborted,
        "bootstrap_transport_failed",
        message,
    )
}

#[cfg(test)]
pub fn discover_remote(
    target: SshTarget,
    authentication: SshAuthenticationMode,
    timeout: Duration,
) -> io::Result<RemoteDiscovery> {
    let socket_path = crate::client::socket_path()?;
    let runtime_directory = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?;
    let user_config = env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".ssh/config"));
    discover_remote_at(
        runtime_directory,
        user_config.as_deref(),
        target,
        authentication,
        timeout,
        OsStr::new("ssh"),
    )
}

fn discover_remote_in_session(
    session: &BootstrapSession,
    timeout: Duration,
) -> io::Result<RemoteDiscovery> {
    let run = |probe: RemoteProbe| run_bounded_command(session.command(probe.command()), timeout);
    let platform = RemotePlatform::parse_probe(&run(RemoteProbe::Platform)?.stdout)?;
    let executables = parse_executable_probe(&run(RemoteProbe::Executables)?.stdout)?;
    let (install_destination, recovery) =
        parse_install_destination_state(&run(RemoteProbe::InstallDestination)?.stdout)?;
    Ok(RemoteDiscovery {
        platform,
        executables,
        install_destination,
        recovery,
    })
}

#[cfg(test)]
pub fn plan_remote_bootstrap(
    target: SshTarget,
    authentication: SshAuthenticationMode,
    timeout: Duration,
) -> io::Result<RemoteBootstrapPlan> {
    let socket_path = crate::client::socket_path()?;
    let runtime_directory = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?;
    let user_config = env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".ssh/config"));
    plan_remote_bootstrap_at(
        runtime_directory,
        user_config.as_deref(),
        target,
        authentication,
        timeout,
        OsStr::new("ssh"),
    )
}

#[cfg(test)]
fn plan_remote_bootstrap_at(
    runtime_directory: &Path,
    user_config: Option<&Path>,
    target: SshTarget,
    authentication: SshAuthenticationMode,
    timeout: Duration,
    program: &OsStr,
) -> io::Result<RemoteBootstrapPlan> {
    let discovery = discover_remote_at(
        runtime_directory,
        user_config,
        target.clone(),
        authentication,
        timeout,
        program,
    )?;
    let selection = inspect_remote_helpers_at(
        runtime_directory,
        user_config,
        target.clone(),
        &discovery.executables,
        authentication,
        timeout,
        program,
    )?;
    if let Some(helper) = selection.compatible {
        return Ok(RemoteBootstrapPlan::Ready(helper));
    }
    if let Some(error) = remote_recovery_error(discovery.recovery) {
        return Err(error);
    }
    let reason = if discovery.executables.is_empty() {
        RemoteInstallReason::Missing
    } else if selection.incompatible == discovery.executables.len() {
        RemoteInstallReason::Upgrade
    } else {
        return Err(io::Error::other(
            "remote Boomux candidates could not be verified; check remote executable access and SSH transport before retrying",
        ));
    };
    let source = select_install_source(discovery.platform)?;
    let upgrade_helper = (reason == RemoteInstallReason::Upgrade)
        .then(|| selection.incompatible_executables.first().cloned())
        .flatten();
    Ok(RemoteBootstrapPlan::Install(RemoteInstallPlan {
        target,
        destination: discovery.install_destination,
        source,
        reason,
        bootstrap_id: None,
        upgrade_helper,
        intent: RemoteInstallIntent::AutomaticCompatibility,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteDaemonStatus {
    Absent,
    Present {
        protocol_version: u32,
        pid: Option<u32>,
        executable: Option<RemoteExecutable>,
        socket_device: Option<u64>,
        socket_inode: Option<u64>,
    },
}

impl RemoteDaemonStatus {
    fn restart_required(&self) -> bool {
        matches!(
            self,
            Self::Present {
                protocol_version, ..
            }
                if !(federation_protocol_floor()..=protocol::PROTOCOL_VERSION)
                    .contains(protocol_version)
        )
    }

    fn proves_executable(&self, destination: &RemoteExecutable) -> bool {
        matches!(
            self,
            Self::Present {
                pid: Some(_),
                executable: Some(executable),
                socket_device: Some(_),
                socket_inode: Some(_),
                ..
            } if executable == destination
        )
    }
}

fn remote_daemon_status_in_session(
    session: &BootstrapSession,
    executable: &RemoteExecutable,
    timeout: Duration,
) -> io::Result<RemoteDaemonStatus> {
    let status = run_bounded_command(
        session.command(&remote_daemon_status_command_for(executable)),
        timeout,
    )?;
    parse_remote_daemon_status(&status.stdout)
}

fn prove_remote_daemon_absent(session: &BootstrapSession, timeout: Duration) -> io::Result<()> {
    let output = run_bounded_command(session.command(&remote_daemon_presence_command()), timeout)
        .map_err(|_| {
            install_presence_required(
                "remote daemon absence could not be proven; inspect or remove the runtime socket manually before installing Boomux",
            )
        })?;
    let fields = parse_nul_fields(&output.stdout, DAEMON_PRESENCE_PREFIX).map_err(|_| {
        install_presence_required(
            "remote daemon absence could not be proven; inspect or remove the runtime socket manually before installing Boomux",
        )
    })?;
    if fields == [b"absent"] {
        Ok(())
    } else {
        Err(install_presence_required(
            "a remote daemon socket already exists; stop or recover that daemon and remove only a confirmed stale socket before installing Boomux",
        ))
    }
}

fn parse_remote_daemon_status(stdout: &[u8]) -> io::Result<RemoteDaemonStatus> {
    if stdout.starts_with(DAEMON_STATUS_PREFIX) {
        let fields = parse_nul_fields(stdout, DAEMON_STATUS_PREFIX)?;
        return if fields == [b"absent"] {
            Ok(RemoteDaemonStatus::Absent)
        } else {
            Err(invalid_probe(
                "remote daemon status returned an invalid result",
            ))
        };
    }
    let value: serde_json::Value = serde_json::from_slice(stdout)
        .map_err(|_| invalid_probe("remote daemon status returned invalid JSON"))?;
    if value.get("schema").and_then(serde_json::Value::as_str) != Some("boomux.cli/v1")
        || value.get("command").and_then(serde_json::Value::as_str) != Some("daemon.status")
    {
        return Err(invalid_probe(
            "remote daemon status returned an invalid envelope",
        ));
    }
    let version = value
        .pointer("/data/protocol_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| invalid_probe("remote daemon status omitted its protocol version"))?;
    let pid = value
        .pointer("/data/pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0);
    let executable = value
        .pointer("/data/executable")
        .and_then(serde_json::Value::as_str)
        .and_then(|executable| RemoteExecutable::parse(executable).ok());
    let socket_device = value
        .pointer("/data/socket_device")
        .and_then(serde_json::Value::as_u64);
    let socket_inode = value
        .pointer("/data/socket_inode")
        .and_then(serde_json::Value::as_u64);
    Ok(RemoteDaemonStatus::Present {
        protocol_version: version,
        pid,
        executable,
        socket_device,
        socket_inode,
    })
}

#[cfg(test)]
pub fn connect_remote(
    target: SshTarget,
    helper: CompatibleRemoteHelper,
    authentication: SshAuthenticationMode,
    timeout: Duration,
) -> io::Result<RemoteConnection> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH connection timeout is outside the supported bound",
        ));
    }
    let invocation = SshInvocation::prepare(target, helper.executable.clone(), authentication)?;
    connect_remote_command(invocation.command(), helper, timeout, None)
}

fn connect_remote_in_session(
    session: BootstrapSession,
    helper: CompatibleRemoteHelper,
    timeout: Duration,
) -> io::Result<RemoteConnection> {
    let command = session.command(&remote_helper_command(&helper.executable));
    connect_remote_command(command, helper, timeout, Some(session))
}

fn connect_remote_command(
    mut command: Command,
    helper: CompatibleRemoteHelper,
    timeout: Duration,
    bootstrap_session: Option<BootstrapSession>,
) -> io::Result<RemoteConnection> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH connection timeout is outside the supported bound",
        ));
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let pid = i32::try_from(child.id()).map_err(|error| {
        let _ = child.kill();
        let _ = child.wait();
        io::Error::other(format!("child PID overflow: {error}"))
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("SSH helper stdin was not captured"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("SSH helper stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("SSH helper stderr was not captured"))?;
    let stderr_reader = match spawn_bounded_reader(stderr, MAX_PROBE_STDERR_BYTES, "stderr") {
        Ok(reader) => reader,
        Err(error) => {
            let _ = kill_process_group(pid, &mut child);
            return Err(error);
        }
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    let handshake_worker = match thread::Builder::new()
        .name("boomux-ssh-handshake".into())
        .spawn(move || {
            let mut stdout = stdout;
            let result = crate::federation::read_handshake(&mut stdout);
            let _ = sender.send((result, stdout));
        }) {
        Ok(worker) => worker,
        Err(error) => {
            let _ = kill_process_group(pid, &mut child);
            let _ = join_bounded_reader(stderr_reader);
            return Err(error);
        }
    };
    let received = receiver.recv_timeout(timeout);
    let (handshake, stdout) = match received {
        Ok((result, stdout)) => (result, stdout),
        Err(_) => {
            let _ = kill_process_group(pid, &mut child);
            handshake_worker
                .join()
                .map_err(|_| io::Error::other("SSH handshake worker panicked"))?;
            let _ = join_bounded_reader(stderr_reader);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH helper handshake timed out",
            ));
        }
    };
    handshake_worker
        .join()
        .map_err(|_| io::Error::other("SSH handshake worker panicked"))?;
    let handshake = match handshake {
        Ok(handshake) => handshake,
        Err(error) => {
            let _ = kill_process_group(pid, &mut child);
            let (stderr, truncated) = join_bounded_reader(stderr_reader)?;
            if !truncated && let Some(runtime_error) = runtime_stage_failure(None, &stderr) {
                return Err(runtime_error);
            }
            return Err(error);
        }
    };
    if let Err(error) = validate_live_handshake(&helper.handshake, &handshake) {
        let _ = kill_process_group(pid, &mut child);
        let _ = join_bounded_reader(stderr_reader);
        return Err(error);
    }
    Ok(RemoteConnection {
        child,
        pid,
        stdin: Some(stdin),
        stdout,
        stderr_reader: Some(stderr_reader),
        executable: helper.executable,
        handshake,
        _bootstrap_session: bootstrap_session,
    })
}

fn validate_live_handshake(
    expected: &FederationHandshake,
    actual: &FederationHandshake,
) -> io::Result<()> {
    if !(federation_protocol_floor()..=protocol::PROTOCOL_VERSION)
        .contains(&actual.core_protocol_version)
    {
        return Err(invalid_probe(
            "remote helper reported an incompatible core protocol",
        ));
    }
    if actual.node_id != expected.node_id {
        return Err(classified_error(
            io::ErrorKind::PermissionDenied,
            "node_identity_changed",
            "remote helper identity changed after bootstrap",
        ));
    }
    Ok(())
}

fn select_install_source(platform: RemotePlatform) -> io::Result<RemoteInstallSource> {
    if platform.matches_local() {
        let path = env::current_exe()?;
        let bytes = read_bounded_file(&path)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        return Ok(RemoteInstallSource::CurrentBinary {
            path,
            sha256,
            bytes,
        });
    }
    let target = platform.release_target().ok_or_else(|| {
        classified_error(
            io::ErrorKind::Unsupported,
            "bootstrap_unsupported_platform",
            "no Boomux release asset supports the remote platform",
        )
    })?;
    let release = select_published_release(target)?;
    let bytes = download_release_binary(target, release.tag)?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    Ok(RemoteInstallSource::Release {
        target,
        tag: release.tag.to_owned(),
        sha256,
        bytes,
    })
}

fn select_published_release(target: &str) -> io::Result<&'static PublishedRelease> {
    PUBLISHED_RELEASES
        .iter()
        .rev()
        .find(|release| {
            release.target == target
                && release.protocol_version >= federation_protocol_floor()
                && release.protocol_version <= protocol::PROTOCOL_VERSION
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "no published Boomux release for {target} is compatible with local protocol {}; build for the remote target and manually stream the current development binary",
                    protocol::PROTOCOL_VERSION
                ),
            )
        })
}

#[derive(Debug)]
struct PublishedRelease {
    tag: &'static str,
    target: &'static str,
    protocol_version: u32,
}

// Keep this matrix explicit: package versions on development branches do not
// prove that an asset exists or that its wire protocol matches this source.
const PUBLISHED_RELEASES: &[PublishedRelease] = &[
    PublishedRelease {
        tag: "v0.18.1",
        target: "x86_64-unknown-linux-gnu",
        protocol_version: 27,
    },
    PublishedRelease {
        tag: "v0.30.3",
        target: "x86_64-unknown-linux-gnu",
        protocol_version: 44,
    },
];

const fn federation_protocol_floor() -> u32 {
    protocol::ProtocolFeature::FederationChannel.minimum_version()
}

fn read_bounded_file(path: &Path) -> io::Result<Vec<u8>> {
    read_bounded_file_with_hook(path, || {})
}

fn read_bounded_file_with_hook(path: &Path, after_metadata: impl FnOnce()) -> io::Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| {
            classified_error(
                io::ErrorKind::InvalidData,
                "bootstrap_install_failed",
                "Boomux install source could not be opened safely",
            )
        })?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() == 0 || before.len() > MAX_RELEASE_BYTES {
        return Err(classified_error(
            io::ErrorKind::InvalidData,
            "bootstrap_install_failed",
            "Boomux install source is not a bounded regular file",
        ));
    }
    after_metadata();
    let mut bytes = Vec::with_capacity(usize::try_from(before.len()).unwrap_or(0));
    Read::by_ref(&mut file)
        .take(MAX_RELEASE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() as u64 != before.len()
        || bytes.len() as u64 > MAX_RELEASE_BYTES
        || file_metadata_changed(&before, &after)
    {
        return Err(classified_error(
            io::ErrorKind::InvalidData,
            "bootstrap_install_failed",
            "Boomux install source changed while it was being pinned",
        ));
    }
    Ok(bytes)
}

fn file_metadata_changed(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
}

fn download_release_binary(target: &str, tag: &str) -> io::Result<Vec<u8>> {
    let socket_path = crate::client::socket_path()?;
    let parent = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?;
    secure_runtime_directory(parent)?;
    let directory = parent.join(format!("release-{}", Uuid::new_v4()));
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let result = (|| {
        let archive_name = format!("boomux-{tag}-{target}.tar.gz");
        let archive = directory.join(&archive_name);
        let checksum = directory.join(format!("{archive_name}.sha256"));
        let base = format!("https://github.com/gardnmi/boomux/releases/download/{tag}");
        for (url, destination, maximum_size) in [
            (
                format!("{base}/{archive_name}"),
                &archive,
                MAX_RELEASE_BYTES,
            ),
            (format!("{base}/{archive_name}.sha256"), &checksum, 1024),
        ] {
            let status = Command::new("curl")
                .args([
                    "--fail",
                    "--location",
                    "--silent",
                    "--show-error",
                    "--output",
                ])
                .arg(destination)
                .arg("--max-filesize")
                .arg(maximum_size.to_string())
                .arg(url)
                .status()?;
            if !status.success() {
                return Err(io::Error::other("could not download Boomux release asset"));
            }
        }
        if fs::metadata(&archive)?.len() > MAX_RELEASE_BYTES {
            return Err(invalid_probe(
                "Boomux release archive exceeds the size limit",
            ));
        }
        verify_release_checksum(&archive, &checksum, &archive_name)?;
        let member = format!("boomux-{tag}-{target}/boomux");
        let status = Command::new("tar")
            .args(["-xzf"])
            .arg(&archive)
            .args(["-C"])
            .arg(&directory)
            .arg(&member)
            .status()?;
        if !status.success() {
            return Err(io::Error::other("could not extract Boomux release asset"));
        }
        read_bounded_file(&directory.join(member))
    })();
    let _ = fs::remove_dir_all(&directory);
    result
}

fn verify_release_checksum(archive: &Path, checksum: &Path, archive_name: &str) -> io::Result<()> {
    let checksum = fs::read_to_string(checksum)?;
    if checksum.len() > 1024 {
        return Err(invalid_probe(
            "Boomux release checksum exceeds the size limit",
        ));
    }
    let mut fields = checksum.split_ascii_whitespace();
    let expected = fields
        .next()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| invalid_probe("Boomux release checksum is invalid"))?;
    let name = fields
        .next()
        .map(|value| value.trim_start_matches('*'))
        .ok_or_else(|| invalid_probe("Boomux release checksum has no filename"))?;
    if name != archive_name || fields.next().is_some() {
        return Err(invalid_probe(
            "Boomux release checksum names an unexpected asset",
        ));
    }
    let mut file = fs::File::open(archive)?;
    let mut digest = Sha256::new();
    io::copy(&mut file, &mut digest)?;
    let actual = format!("{:x}", digest.finalize());
    if actual != expected.to_ascii_lowercase() {
        return Err(invalid_probe("Boomux release checksum did not match"));
    }
    Ok(())
}

#[cfg(test)]
pub fn find_compatible_remote_helper(
    target: SshTarget,
    executables: &[RemoteExecutable],
    authentication: SshAuthenticationMode,
    timeout: Duration,
) -> io::Result<CompatibleRemoteHelper> {
    inspect_remote_helpers(target, executables, authentication, timeout)?
        .compatible
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no discovered remote Boomux executable is federation-compatible",
            )
        })
}

struct RemoteHelperInspection {
    compatible: Option<CompatibleRemoteHelper>,
    incompatible: usize,
    incompatible_executables: Vec<RemoteExecutable>,
}

fn inspect_remote_helpers_in_session(
    session: &BootstrapSession,
    executables: &[RemoteExecutable],
    timeout: Duration,
) -> io::Result<RemoteHelperInspection> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH helper selection timeout is outside the supported bound",
        ));
    }
    let deadline = Instant::now() + timeout;
    let mut compatible: Option<CompatibleRemoteHelper> = None;
    let mut incompatible = 0;
    let mut incompatible_executables = Vec::new();
    let mut indeterminate = 0;
    let mut first_indeterminate_error = None;
    for (index, executable) in executables.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH helper selection timed out",
            ));
        }
        let candidates_left = u32::try_from(executables.len() - index).unwrap_or(u32::MAX);
        let candidate_budget = remaining / candidates_left;
        if candidate_budget.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH helper selection timed out",
            ));
        }
        let candidate_deadline = Instant::now() + candidate_budget;
        let helper_command = remote_helper_command(executable);
        match run_helper_probe_command(session.command(&helper_command), candidate_budget) {
            Ok(handshake) => {
                if let Some(selected) = &compatible {
                    if selected.handshake.node_id != handshake.node_id {
                        return Err(classified_error(
                            io::ErrorKind::PermissionDenied,
                            "node_identity_conflict",
                            "discovered remote Boomux executables reported different Node identities",
                        ));
                    }
                } else {
                    compatible = Some(CompatibleRemoteHelper {
                        executable: executable.clone(),
                        handshake,
                        bootstrap_id: Some(session.id),
                    });
                }
            }
            Err(helper_error) => {
                let remaining = candidate_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    indeterminate += 1;
                    first_indeterminate_error.get_or_insert(helper_error);
                    continue;
                }
                let version = run_bounded_command(
                    session.command(&format!(
                        "{} --version",
                        quote_posix_shell(executable.as_str())
                    )),
                    remaining,
                );
                let published_version = version
                    .as_ref()
                    .ok()
                    .and_then(|output| published_protocol_from_version_output(&output.stdout));
                if matches!(
                    helper_error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::Unsupported
                ) && published_version
                    .is_some_and(|version| version < federation_protocol_floor())
                {
                    incompatible += 1;
                    incompatible_executables.push(executable.clone());
                } else {
                    indeterminate += 1;
                    let error = if matches!(
                        helper_error.kind(),
                        io::ErrorKind::InvalidData | io::ErrorKind::PermissionDenied
                    ) {
                        helper_error
                    } else {
                        version.err().unwrap_or(helper_error)
                    };
                    first_indeterminate_error.get_or_insert(error);
                }
            }
        }
    }
    if compatible.is_none() && indeterminate > 0 {
        return Err(first_indeterminate_error.unwrap_or_else(|| {
            io::Error::other(
                "remote Boomux candidate failed compatibility verification; refusing remote modification",
            )
        }));
    }
    Ok(RemoteHelperInspection {
        compatible,
        incompatible,
        incompatible_executables,
    })
}

#[cfg(test)]
fn inspect_remote_helpers(
    target: SshTarget,
    executables: &[RemoteExecutable],
    authentication: SshAuthenticationMode,
    timeout: Duration,
) -> io::Result<RemoteHelperInspection> {
    let socket_path = crate::client::socket_path()?;
    let runtime_directory = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?;
    let user_config = env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".ssh/config"));
    inspect_remote_helpers_at(
        runtime_directory,
        user_config.as_deref(),
        target,
        executables,
        authentication,
        timeout,
        OsStr::new("ssh"),
    )
}

#[cfg(test)]
fn find_compatible_remote_helper_at(
    runtime_directory: &Path,
    user_config: Option<&Path>,
    target: SshTarget,
    executables: &[RemoteExecutable],
    authentication: SshAuthenticationMode,
    timeout: Duration,
    program: &OsStr,
) -> io::Result<CompatibleRemoteHelper> {
    inspect_remote_helpers_at(
        runtime_directory,
        user_config,
        target,
        executables,
        authentication,
        timeout,
        program,
    )?
    .compatible
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no discovered remote Boomux executable is federation-compatible",
        )
    })
}

#[cfg(test)]
fn inspect_remote_helpers_at(
    runtime_directory: &Path,
    user_config: Option<&Path>,
    target: SshTarget,
    executables: &[RemoteExecutable],
    authentication: SshAuthenticationMode,
    timeout: Duration,
    program: &OsStr,
) -> io::Result<RemoteHelperInspection> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH helper selection timeout is outside the supported bound",
        ));
    }
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH helper selection timeout overflow",
        )
    })?;
    let mut compatible: Option<CompatibleRemoteHelper> = None;
    let mut incompatible = 0;
    let mut incompatible_executables = Vec::new();
    let mut indeterminate = 0;
    let mut first_indeterminate_error = None;
    for (index, executable) in executables.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH helper selection timed out",
            ));
        }
        let candidates_left = u32::try_from(executables.len() - index).unwrap_or(u32::MAX);
        let candidate_budget = remaining / candidates_left;
        if candidate_budget.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH helper selection timed out",
            ));
        }
        let candidate_deadline = Instant::now() + candidate_budget;
        let invocation = SshInvocation::prepare_command_with_program_at(
            runtime_directory,
            user_config,
            target.clone(),
            remote_helper_command(executable),
            authentication,
            program,
        )?;
        match invocation.verify_helper(candidate_budget) {
            Ok(handshake) => {
                if let Some(selected) = &compatible {
                    if selected.handshake.node_id != handshake.node_id {
                        return Err(classified_error(
                            io::ErrorKind::PermissionDenied,
                            "node_identity_conflict",
                            "discovered remote Boomux executables reported different Node identities",
                        ));
                    }
                } else {
                    compatible = Some(CompatibleRemoteHelper {
                        executable: executable.clone(),
                        handshake,
                        bootstrap_id: None,
                    });
                }
            }
            Err(helper_error) => {
                let remaining = candidate_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    indeterminate += 1;
                    first_indeterminate_error.get_or_insert(helper_error);
                    continue;
                }
                let version = SshInvocation::prepare_command_with_program_at(
                    runtime_directory,
                    user_config,
                    target.clone(),
                    format!("{} --version", quote_posix_shell(executable.as_str())),
                    authentication,
                    program,
                )?
                .run_probe(remaining);
                let published_version = version
                    .as_ref()
                    .ok()
                    .and_then(|output| published_protocol_from_version_output(&output.stdout));
                if matches!(
                    helper_error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::Unsupported
                ) && published_version
                    .is_some_and(|version| version < federation_protocol_floor())
                {
                    incompatible += 1;
                    incompatible_executables.push(executable.clone());
                } else {
                    indeterminate += 1;
                    let error = if matches!(
                        helper_error.kind(),
                        io::ErrorKind::InvalidData | io::ErrorKind::PermissionDenied
                    ) {
                        helper_error
                    } else {
                        version.err().unwrap_or(helper_error)
                    };
                    first_indeterminate_error.get_or_insert(error);
                }
            }
        }
    }
    if compatible.is_none() && indeterminate > 0 {
        return Err(first_indeterminate_error.unwrap_or_else(|| {
            io::Error::other(
                "remote Boomux candidate failed compatibility verification; refusing remote modification",
            )
        }));
    }
    Ok(RemoteHelperInspection {
        compatible,
        incompatible,
        incompatible_executables,
    })
}

fn published_protocol_from_version_output(output: &[u8]) -> Option<u32> {
    if output.len() > 128 {
        return None;
    }
    let version = std::str::from_utf8(output)
        .ok()?
        .strip_prefix("boomux ")?
        .strip_suffix('\n')?;
    if version.is_empty()
        || version.len() > 32
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    match version {
        "0.2.0" | "0.3.0" | "0.4.0" | "0.4.1" => Some(16),
        "0.4.2" => Some(17),
        "0.5.0" => Some(18),
        "0.5.1" => Some(19),
        "0.6.0" | "0.7.0" | "0.8.0" | "0.9.0" | "0.9.1" | "0.10.0" | "0.10.1" | "0.11.0"
        | "0.12.0" | "0.13.0" => Some(20),
        "0.14.0" | "0.14.1" | "0.14.2" | "0.15.0" => Some(21),
        "0.16.0" | "0.17.0" | "0.18.0" | "0.18.1" => Some(27),
        _ => None,
    }
}

#[cfg(test)]
fn discover_remote_at(
    runtime_directory: &Path,
    user_config: Option<&Path>,
    target: SshTarget,
    authentication: SshAuthenticationMode,
    timeout: Duration,
    program: &OsStr,
) -> io::Result<RemoteDiscovery> {
    let run = |probe: RemoteProbe| -> io::Result<SshProbeOutput> {
        SshInvocation::prepare_command_with_program_at(
            runtime_directory,
            user_config,
            target.clone(),
            probe.command().to_owned(),
            authentication,
            program,
        )?
        .run_probe(timeout)
    };
    let platform = RemotePlatform::parse_probe(&run(RemoteProbe::Platform)?.stdout)?;
    let executables = parse_executable_probe(&run(RemoteProbe::Executables)?.stdout)?;
    let (install_destination, recovery) =
        parse_install_destination_state(&run(RemoteProbe::InstallDestination)?.stdout)?;
    Ok(RemoteDiscovery {
        platform,
        executables,
        install_destination,
        recovery,
    })
}

impl Drop for SshInvocation {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn secure_runtime_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Boomux runtime path is not an owned directory",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn quote_posix_shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn quote_ssh_config_path(path: &Path) -> io::Result<String> {
    let value = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH configuration path must be valid UTF-8",
        )
    })?;
    if value.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH configuration path contains control characters",
        ));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn validate_option_path(path: &Path, label: &str) -> io::Result<()> {
    let value = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} path must be valid UTF-8"),
        )
    })?;
    if value.chars().any(char::is_control)
        || value.chars().any(char::is_whitespace)
        || value.contains('%')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} path contains unsafe option characters"),
        ));
    }
    Ok(())
}

fn effective_uid() -> u32 {
    // `geteuid` has no arguments, pointers, or caller safety requirements.
    unsafe { libc::geteuid() }
}

fn parse_nul_fields<'a>(output: &'a [u8], prefix: &[u8]) -> io::Result<Vec<&'a [u8]>> {
    if output.len() > MAX_PROBE_OUTPUT_BYTES {
        return Err(invalid_probe("remote probe output exceeds the size limit"));
    }
    let mut fields = output.split(|byte| *byte == 0);
    if fields.next() != Some(prefix) {
        return Err(invalid_probe("remote probe returned an invalid header"));
    }
    let mut values = fields.collect::<Vec<_>>();
    if values.last() != Some(&&b""[..]) {
        return Err(invalid_probe("remote probe output is not NUL terminated"));
    }
    values.pop();
    if values.iter().any(|value| value.is_empty()) {
        return Err(invalid_probe("remote probe returned an empty field"));
    }
    Ok(values)
}

fn invalid_probe(message: &'static str) -> io::Error {
    classified_error(
        io::ErrorKind::InvalidData,
        "bootstrap_malformed_helper",
        message,
    )
}

fn run_bounded_command(mut command: Command, timeout: Duration) -> io::Result<SshProbeOutput> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH probe timeout is outside the supported bound",
        ));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The child has not executed remote or user code; `setsid` creates a process
    // group that can be terminated and reaped as one bounded probe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let pid = i32::try_from(child.id()).map_err(|_| io::Error::other("child PID overflow"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("SSH probe stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("SSH probe stderr was not captured"))?;
    let stdout_reader = spawn_bounded_reader(stdout, MAX_PROBE_OUTPUT_BYTES, "stdout")?;
    let stderr_reader = spawn_bounded_reader(stderr, MAX_PROBE_STDERR_BYTES, "stderr")?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "SSH probe timeout overflow"))?;

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            // Negative PID addresses the process group created by `setsid`.
            if unsafe { libc::kill(-pid, libc::SIGKILL) } == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
            let _ = child.wait();
            break None;
        }
        thread::sleep(CHILD_POLL_INTERVAL);
    };

    let (stdout, stdout_truncated) = join_bounded_reader(stdout_reader)?;
    let (stderr, stderr_truncated) = join_bounded_reader(stderr_reader)?;
    if status.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "SSH probe timed out",
        ));
    }
    if stdout_truncated || stderr_truncated {
        return Err(invalid_probe("SSH probe output exceeds the size limit"));
    }
    let status = status.expect("checked above");
    if !status.success() {
        return Err(classify_ssh_command_failure(status.code(), &stderr));
    }
    Ok(SshProbeOutput { stdout, stderr })
}

fn classify_ssh_command_failure(status: Option<i32>, stderr: &[u8]) -> io::Error {
    let lower = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if lower.contains("permission denied")
        || lower.contains("authentication failed")
        || lower.contains("host key verification failed")
    {
        return classified_error(
            io::ErrorKind::PermissionDenied,
            "bootstrap_authentication_failed",
            "SSH authentication or host-key verification failed",
        );
    }
    if status == Some(73) {
        return classified_error(
            io::ErrorKind::ResourceBusy,
            "busy",
            "another remote Boomux bootstrap transaction is active",
        );
    }
    if status == Some(92) {
        return classified_error(
            io::ErrorKind::Unsupported,
            "bootstrap_unsupported_platform",
            "remote platform does not provide the fixed command layout required for safe bootstrap",
        );
    }
    if let Some(error) = runtime_stage_failure(status, stderr) {
        return error;
    }
    if let Some(detail) = install_stage_failure(status, stderr) {
        return classified_error(io::ErrorKind::Other, "bootstrap_install_failed", detail);
    }
    if status == Some(255) {
        return classified_error(
            io::ErrorKind::ConnectionAborted,
            "bootstrap_transport_failed",
            "SSH transport failed while using the authenticated bootstrap endpoint",
        );
    }
    io::Error::other("SSH remote command failed; verify executable access and remote shell support")
}

fn runtime_stage_failure(status: Option<i32>, stderr: &[u8]) -> Option<io::Error> {
    let (reason, code) = std::str::from_utf8(stderr).ok()?.lines().find_map(|line| {
        let fields = line.strip_prefix(RUNTIME_STAGE_PREFIX)?.strip_prefix(':')?;
        let (reason, code) = fields.split_once(':')?;
        let code = code.parse::<i32>().ok()?;
        (status.is_none() || status == Some(code)).then_some((reason, code))
    })?;
    let message = match (reason, code) {
        ("missing", 88) => {
            "remote runtime discovery failed: XDG_RUNTIME_DIR is unset on a non-Linux host"
        }
        ("invalid", 89) => {
            "remote runtime discovery failed: user identity or XDG_RUNTIME_DIR is invalid"
        }
        ("unsafe", 90) => {
            "remote runtime discovery failed: runtime directory must be an owner-controlled, non-symlink directory with mode 0700"
        }
        ("unsupported", 91) => {
            "remote runtime discovery failed: remote operating system is unsupported"
        }
        _ => return None,
    };
    Some(classified_error(
        io::ErrorKind::NotFound,
        "bootstrap_runtime_unavailable",
        message,
    ))
}

fn install_stage_failure(status: Option<i32>, stderr: &[u8]) -> Option<&'static str> {
    let status = status?;
    let marker = std::str::from_utf8(stderr).ok()?.lines().find_map(|line| {
        let fields = line.strip_prefix(INSTALL_STAGE_PREFIX)?.strip_prefix(':')?;
        let (stage, code) = fields.split_once(':')?;
        (code.parse::<i32>().ok()? == status).then_some(stage)
    })?;
    Some(match marker {
        "home" => "remote Boomux install failed: remote HOME is not an absolute path",
        "directory" => "remote Boomux install failed while creating the owner install directory",
        "lock" => "remote Boomux install failed while acquiring its transaction lock",
        "transaction" => {
            "remote Boomux install failed while creating its private transaction directory"
        }
        "transaction_id" => {
            "remote Boomux install failed because mktemp returned an invalid transaction name"
        }
        "lock_id" => "remote Boomux install failed while recording its transaction identity",
        "stream" => {
            "remote Boomux install failed while writing the streamed binary; check remote free space and quota"
        }
        "mode" => "remote Boomux install failed while making the streamed binary executable",
        "backup" => "remote Boomux install failed while preserving the previous executable",
        "activate" => "remote Boomux install failed while activating the replacement executable",
        "watchdog_spawn" => "remote Boomux install failed while starting the rollback watchdog",
        "watchdog_ready" => {
            "remote Boomux install failed because the rollback watchdog did not become ready"
        }
        "watchdog_pid" => {
            "remote Boomux install failed while recording rollback watchdog ownership"
        }
        "result" => "remote Boomux install failed while returning its transaction result",
        _ => return None,
    })
}

fn run_streaming_command(command: Command, input: Vec<u8>, timeout: Duration) -> io::Result<()> {
    run_streaming_command_capture(command, input, timeout).map(|_| ())
}

fn run_streaming_command_capture(
    mut command: Command,
    input: Vec<u8>,
    timeout: Duration,
) -> io::Result<SshProbeOutput> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH install timeout is outside the supported bound",
        ));
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let pid = i32::try_from(child.id()).map_err(|_| io::Error::other("child PID overflow"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("SSH install stdin was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("SSH install stderr was not captured"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("SSH install stdout was not captured"))?;
    let stdout_reader = spawn_bounded_reader(stdout, MAX_PROBE_OUTPUT_BYTES, "install-stdout")?;
    let stderr_reader = spawn_bounded_reader(stderr, MAX_PROBE_STDERR_BYTES, "stderr")?;
    let writer = thread::Builder::new()
        .name("boomux-ssh-install-input".into())
        .spawn(move || stdin.write_all(&input))?;
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "SSH install timeout overflow")
    })?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            kill_process_group(pid, &mut child)?;
            break None;
        }
        thread::sleep(CHILD_POLL_INTERVAL);
    };
    let write_result = writer
        .join()
        .map_err(|_| io::Error::other("SSH install input worker panicked"))?;
    let (stdout, stdout_truncated) = join_bounded_reader(stdout_reader)?;
    let (stderr, stderr_truncated) = join_bounded_reader(stderr_reader)?;
    if status.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "SSH install timed out",
        ));
    }
    if stdout_truncated || stderr_truncated {
        return Err(invalid_probe("SSH install output exceeds the size limit"));
    }
    let status = status.expect("checked above");
    if !status.success() {
        let error = classify_ssh_command_failure(status.code(), &stderr);
        return if error.kind() == io::ErrorKind::Other
            && error
                .get_ref()
                .and_then(|error| error.downcast_ref::<ClassifiedBootstrapError>())
                .is_none()
        {
            Err(classified_error(
                io::ErrorKind::Other,
                "bootstrap_install_failed",
                "remote Boomux install failed",
            ))
        } else {
            Err(error)
        };
    }
    write_result?;
    Ok(SshProbeOutput { stdout, stderr })
}

fn run_helper_probe_command(
    mut command: Command,
    timeout: Duration,
) -> io::Result<FederationHandshake> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH helper probe timeout is outside the supported bound",
        ));
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The child has not executed remote or user code; `setsid` gives timeout
    // cleanup one process-group boundary.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let pid = i32::try_from(child.id()).map_err(|_| io::Error::other("child PID overflow"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("SSH helper stdin was not captured"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("SSH helper stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("SSH helper stderr was not captured"))?;
    let stderr_reader = spawn_bounded_reader(stderr, MAX_PROBE_STDERR_BYTES, "stderr")?;
    let (result_sender, result_receiver) = mpsc::sync_channel(1);
    let protocol_worker = thread::Builder::new()
        .name("boomux-ssh-helper-probe".into())
        .spawn(move || {
            let result = (|| {
                let handshake = crate::federation::read_handshake(&mut stdout)?;
                if handshake.core_protocol_version < federation_protocol_floor() {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "remote helper reported an older incompatible core protocol",
                    ));
                }
                if handshake.core_protocol_version > protocol::PROTOCOL_VERSION {
                    return Err(classified_error(
                        io::ErrorKind::Unsupported,
                        "unsupported_version",
                        "remote helper reported a newer incompatible core protocol",
                    ));
                }
                protocol::write_message(
                    &mut stdin,
                    &Envelope::with_version(handshake.core_protocol_version, Request::Ping),
                )?;
                let response: Envelope<Response> = protocol::read_message(&mut stdout)?;
                if response.version != handshake.core_protocol_version
                    || response.message != Response::Pong
                {
                    return Err(invalid_probe(
                        "remote helper returned an invalid compatibility response",
                    ));
                }
                Ok(handshake)
            })();
            let _ = result_sender.send(result);
        })?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "SSH probe timeout overflow"))?;
    let result = match result_receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "SSH helper handshake timed out",
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(io::Error::other("SSH helper probe worker stopped"))
        }
    };
    let status = if result.is_err() {
        kill_process_group(pid, &mut child)?;
        None
    } else {
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if Instant::now() >= deadline {
                kill_process_group(pid, &mut child)?;
                break None;
            }
            thread::sleep(CHILD_POLL_INTERVAL);
        };
        // The SSH leader can exit while a descendant still owns a pipe.
        kill_process_group(pid, &mut child)?;
        status
    };
    protocol_worker
        .join()
        .map_err(|_| io::Error::other("SSH helper probe worker panicked"))?;
    let (stderr, stderr_truncated) = join_bounded_reader(stderr_reader)?;
    if stderr_truncated {
        return Err(invalid_probe("SSH helper stderr exceeds the size limit"));
    }
    if let Some(error) = runtime_stage_failure(status.and_then(|status| status.code()), &stderr) {
        return Err(error);
    }
    if result.is_ok() && status.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "SSH helper did not exit before the compatibility deadline",
        ));
    }
    if status.is_some_and(|status| !status.success()) {
        return Err(classify_ssh_command_failure(
            status.and_then(|status| status.code()),
            &stderr,
        ));
    }
    if result.is_err()
        && (String::from_utf8_lossy(&stderr).contains("Permission denied")
            || String::from_utf8_lossy(&stderr).contains("Host key verification failed"))
    {
        return Err(classify_ssh_command_failure(Some(255), &stderr));
    }
    result
}

fn kill_process_group(pid: i32, child: &mut std::process::Child) -> io::Result<()> {
    if unsafe { libc::kill(-pid, libc::SIGKILL) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    let _ = child.wait();
    Ok(())
}

fn spawn_master_stderr_reader(
    mut reader: impl Read + Send + 'static,
    limit: usize,
    mut mirror: Option<Box<dyn StderrMirror>>,
    master_pid: i32,
    deadline: Instant,
) -> io::Result<MasterStderrReader> {
    if let Some(mirror) = mirror.as_ref() {
        let flags = unsafe { libc::fcntl(mirror.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(mirror.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == -1
        {
            return Err(io::Error::last_os_error());
        }
    }
    let (event_sender, events) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("boomux-ssh-master-stderr".into())
        .spawn(move || {
            let mut retained = Vec::with_capacity(limit);
            let mut buffer = [0_u8; 4096];
            loop {
                let count = match reader.read(&mut buffer) {
                    Ok(0) => return Ok((retained, false)),
                    Ok(count) => count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                };
                let mirrored = count.min(limit.saturating_sub(retained.len()));
                if mirrored != 0 {
                    if let Some(mirror) = mirror.as_mut()
                        && write_mirror_with_deadline(
                            mirror.as_mut(),
                            &buffer[..mirrored],
                            deadline,
                        )
                        .is_err()
                    {
                        let _ = event_sender.send(MasterStderrEvent::MirrorFailed);
                        unsafe {
                            libc::kill(-master_pid, libc::SIGKILL);
                        }
                        return Err(io::Error::other("SSH master stderr mirror failed"));
                    }
                    retained.extend_from_slice(&buffer[..mirrored]);
                }
                if mirrored < count {
                    let _ = event_sender.send(MasterStderrEvent::Truncated);
                    unsafe {
                        libc::kill(-master_pid, libc::SIGKILL);
                    }
                    return Ok((retained, true));
                }
            }
        })?;
    Ok(MasterStderrReader { reader, events })
}

fn write_mirror_with_deadline(
    mirror: &mut dyn StderrMirror,
    mut bytes: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        match mirror.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "terminal output closed",
                ));
            }
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "terminal output timed out",
                    ));
                }
                let mut descriptor = libc::pollfd {
                    fd: mirror.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let milliseconds =
                    i32::try_from(remaining.as_millis().min(i32::MAX as u128)).unwrap_or(i32::MAX);
                let status = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
                if status == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "terminal output timed out",
                    ));
                }
                if status == -1 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    mirror.flush()
}

fn spawn_bounded_reader(
    mut reader: impl Read + Send + 'static,
    limit: usize,
    stream: &'static str,
) -> io::Result<BoundedReader> {
    thread::Builder::new()
        .name(format!("boomux-ssh-{stream}"))
        .spawn(move || {
            let mut bytes = Vec::new();
            reader
                .by_ref()
                .take((limit + 1) as u64)
                .read_to_end(&mut bytes)?;
            let truncated = bytes.len() > limit;
            bytes.truncate(limit);
            Ok((bytes, truncated))
        })
}

fn join_bounded_reader(reader: BoundedReader) -> io::Result<(Vec<u8>, bool)> {
    reader
        .join()
        .map_err(|_| io::Error::other("SSH probe output reader panicked"))?
}

#[cfg(test)]
fn command_arguments(command: &Command) -> Vec<OsString> {
    command.get_args().map(OsStr::to_os_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::federation::{FEDERATION_VERSION, FederationConnectionMode};

    const CONTROL_MASTER_SCRIPT: &str = "control=; previous=; master=false; check=false; last=; for arg do last=$arg; case \"$previous\" in -S) control=$arg ;; -O) [ \"$arg\" = check ] && check=true ;; esac; case \"$arg\" in ControlPath=*) control=${arg#ControlPath=} ;; -N) master=true ;; esac; previous=$arg; done; if $master; then : > \"$control.ready\"; trap 'rm -f \"$control.ready\"' EXIT HUP INT TERM; while :; do sleep 60; done; fi; if $check; then [ -e \"$control.ready\" ]; exit; fi; case \"$last\" in *'boomux-install-activation-v1'*) cat >/dev/null; printf 'boomux-install-activation-v1\\0activated\\0'; exit ;; *'boomux-install-transaction-v1'*) IFS= read -r boomux_test_txn; printf() { case \"$1\" in boomux-install-transaction-v1*) command printf 'boomux-install-transaction-v1\\0%s\\0' \"$boomux_test_txn\" ;; *) command printf \"$@\" ;; esac; } ;; *'prior_daemon.next'*|*': > \"$transaction/daemon_contacted\"'*|*'lease.next'*) cat >/dev/null; exit ;; esac";
    const CONTROL_MASTER_ONLY_SCRIPT: &str = "control=; previous=; master=false; check=false; for arg do case \"$previous\" in -S) control=$arg ;; -O) [ \"$arg\" = check ] && check=true ;; esac; case \"$arg\" in ControlPath=*) control=${arg#ControlPath=} ;; -N) master=true ;; esac; previous=$arg; done; if $master; then : > \"$control.ready\"; trap 'rm -f \"$control.ready\"' EXIT HUP INT TERM; while :; do sleep 60; done; fi; if $check; then [ -e \"$control.ready\" ]; exit; fi";

    fn add_fake_daemon_identity(script: &Path, executable: &str) {
        let contents = fs::read_to_string(script).unwrap();
        let contents = contents
            .replace(
                "\"protocol_version\":21}",
                &format!(
                    "\"protocol_version\":21,\"pid\":123,\"executable\":{},\"socket_device\":1,\"socket_inode\":1}}",
                    serde_json::to_string(executable).unwrap()
                ),
            )
            .replace(
                "\"protocol_version\":38}",
                &format!(
                    "\"protocol_version\":38,\"pid\":123,\"executable\":{},\"socket_device\":1,\"socket_inode\":1}}",
                    serde_json::to_string(executable).unwrap()
                ),
            );
        fs::write(script, contents).unwrap();
    }

    struct TestDirectory {
        path: PathBuf,
        watchdogs: PathBuf,
    }

    impl std::ops::Deref for TestDirectory {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            &self.path
        }
    }

    impl AsRef<Path> for TestDirectory {
        fn as_ref(&self) -> &Path {
            &self.path
        }
    }

    impl AsRef<OsStr> for TestDirectory {
        fn as_ref(&self) -> &OsStr {
            self.path.as_os_str()
        }
    }

    impl TestDirectory {
        fn reap_watchdogs(&self) {
            let mut pids = fs::read_to_string(&self.watchdogs)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    let (pid, start) = line.split_once('\t')?;
                    Some((pid.parse::<i32>().ok()?, start.to_owned()))
                })
                .collect::<HashSet<_>>();
            let bin = self.path.join(".local/bin");
            if let Ok(entries) = fs::read_dir(&bin) {
                for entry in entries.filter_map(Result::ok) {
                    let pid_file = entry.path().join("watchdog_pid");
                    if let Ok(pid) = fs::read_to_string(pid_file)
                        && let Ok(pid) = pid.trim().parse()
                        && let Some(start) = test_process_start(pid)
                    {
                        pids.insert((pid, start));
                    }
                }
            }
            for (pid, start) in &pids {
                if test_process_start(*pid).as_ref() != Some(start) {
                    continue;
                }
                unsafe {
                    libc::kill(*pid, libc::SIGTERM);
                }
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            while pids
                .iter()
                .any(|(pid, start)| test_process_start(*pid).as_ref() == Some(start))
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(10));
            }
            for (pid, start) in pids {
                if test_process_start(pid).as_ref() == Some(&start) {
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            self.reap_watchdogs();
            let _ = fs::remove_dir_all(&self.path);
            let _ = fs::remove_file(&self.watchdogs);
        }
    }

    fn runtime_directory() -> TestDirectory {
        let id = Uuid::new_v4();
        TestDirectory {
            path: env::temp_dir().join(format!("boomux-ssh-{id}")),
            watchdogs: env::temp_dir().join(format!("boomux-ssh-watchdogs-{id}")),
        }
    }

    #[cfg(target_os = "linux")]
    fn test_process_start(pid: i32) -> Option<String> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let mut fields = stat.rsplit_once(") ")?.1.split_whitespace();
        if fields.next()? == "Z" {
            return None;
        }
        fields.nth(18).map(str::to_owned)
    }

    #[cfg(not(target_os = "linux"))]
    fn test_process_start(pid: i32) -> Option<String> {
        let output = Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "state=", "-o", "lstart="])
            .output()
            .ok()?;
        let fields = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        (output.status.success()
            && fields.len() >= 6
            && !fields.first().is_some_and(|state| state.starts_with('Z')))
        .then(|| fields[1..].join(" "))
        .filter(|start| !start.is_empty())
    }

    fn local_shell_command(shell: &str, home: &Path) -> Command {
        let runtime = home.join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let shell = match shell {
            "sh" => "/bin/sh",
            "bash" => "/bin/bash",
            _ => panic!("unsupported test shell: {shell}"),
        };
        let mut command = Command::new(shell);
        command
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", runtime)
            .env("XDG_STATE_HOME", home.join("state"))
            .current_dir(home);
        command
    }

    fn record_watchdog(home: &Path, transaction: &InstallTransactionId) {
        let pid = fs::read_to_string(
            home.join(".local/bin")
                .join(&transaction.0)
                .join("watchdog_pid"),
        )
        .unwrap();
        let pid = pid.trim().parse::<i32>().unwrap();
        let start = test_process_start(pid).unwrap();
        let id = home.file_name().unwrap().to_string_lossy();
        let registry = env::temp_dir().join(format!(
            "boomux-ssh-watchdogs-{}",
            id.trim_start_matches("boomux-ssh-")
        ));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(registry)
            .unwrap();
        writeln!(file, "{pid}\t{start}").unwrap();
    }

    fn gate_watchdog(command: &str) -> String {
        let gated = command.replacen(
            "while [ -d \"$lock\" ]; do /bin/sleep 1;",
            "while [ -d \"$lock\" ]; do test_watchdog_tick;",
            1,
        );
        assert_ne!(gated, command);
        format!(
            "test_watchdog_tick() {{ while [ ! -e \"$HOME/watchdog-tick\" ]; do /bin/sleep 0.01; done; /bin/sleep 0.01; }}; {gated}"
        )
    }

    fn delay_watchdog_readiness(command: &str) -> String {
        let delayed = command.replacen(
            ": > \"$transaction/watchdog_ready\";",
            "/bin/sleep 1; : > \"$transaction/watchdog_ready\";",
            1,
        );
        assert_ne!(delayed, command);
        delayed
    }

    fn shell_printf(bytes: &[u8]) -> String {
        let escaped = bytes
            .iter()
            .map(|byte| format!("\\{byte:03o}"))
            .collect::<String>();
        format!("printf '{escaped}'")
    }

    fn write_master_stderr_ssh(runtime: &Path, master_body: &str) -> PathBuf {
        let ssh = runtime.join("ssh-master-stderr");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\ncontrol=; previous=; master=false; check=false\nfor arg do\n  case \"$previous\" in -S) control=$arg ;; -O) [ \"$arg\" = check ] && check=true ;; esac\n  case \"$arg\" in ControlPath=*) control=${{arg#ControlPath=}} ;; -N) master=true ;; esac\n  previous=$arg\ndone\nif $master; then\n  {master_body}\nfi\nif $check; then [ -e \"$control.ready\" ]; exit; fi\nexit 64\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        ssh
    }

    fn run_local_install(home: &Path, bytes: &[u8]) -> InstallTransactionId {
        run_local_install_with_shell(home, bytes, "sh")
    }

    fn run_local_install_with_shell(
        home: &Path,
        bytes: &[u8],
        shell: &str,
    ) -> InstallTransactionId {
        let transaction = run_local_upload_with_command(home, bytes, shell, REMOTE_INSTALL_COMMAND);
        run_local_activation(home, shell, &transaction, RemoteInstallReason::Missing);
        transaction
    }

    fn run_local_upload_with_command(
        home: &Path,
        bytes: &[u8],
        shell: &str,
        command_text: &str,
    ) -> InstallTransactionId {
        let transaction = InstallTransactionId::generate();
        let mut command = local_shell_command(shell, home);
        command.args(["-c", command_text]);
        let output = run_streaming_command_capture(
            command,
            transaction.upload_input(bytes),
            Duration::from_secs(15),
        )
        .unwrap();
        assert_eq!(
            InstallTransactionId::parse_probe(&output.stdout).unwrap(),
            transaction
        );
        record_watchdog(home, &transaction);
        transaction
    }

    fn run_local_activation(
        home: &Path,
        shell: &str,
        transaction: &InstallTransactionId,
        reason: RemoteInstallReason,
    ) {
        let mut activate = local_shell_command(shell, home);
        activate.args(["-c", REMOTE_INSTALL_ACTIVATE_COMMAND]);
        let output = run_streaming_command_capture(
            activate,
            transaction.activation_input(
                reason,
                &RemoteDaemonStatus::Present {
                    protocol_version: protocol::PROTOCOL_VERSION,
                    pid: Some(1),
                    executable: Some(RemoteExecutable::parse("/tmp/test-destination").unwrap()),
                    socket_device: Some(1),
                    socket_inode: Some(1),
                },
            ),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(parse_install_activation(&output.stdout).unwrap());
    }

    #[test]
    fn exact_install_script_runs_under_posix_sh_and_bash() {
        for shell in ["sh", "bash"] {
            let directory = runtime_directory();
            fs::create_dir_all(directory.join(".local/bin")).unwrap();
            let destination = directory.join(".local/bin/boomux");
            fs::write(&destination, b"previous").unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
            let previous_metadata = fs::metadata(&destination).unwrap();
            let transaction = run_local_install_with_shell(&directory, b"replacement", shell);
            assert_eq!(fs::read(&destination).unwrap(), b"replacement");
            let backup = directory
                .join(".local/bin")
                .join(&transaction.0)
                .join("backup");
            let backup_metadata = fs::metadata(&backup).unwrap();
            assert_eq!(fs::read(&backup).unwrap(), b"previous");
            assert_eq!(backup_metadata.mode(), previous_metadata.mode());
            assert_eq!(backup_metadata.uid(), previous_metadata.uid());
            assert_eq!(backup_metadata.gid(), previous_metadata.gid());
            assert_eq!(backup_metadata.mtime(), previous_metadata.mtime());
            run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
            assert_eq!(fs::read(&destination).unwrap(), b"previous");
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn upgrade_activation_derives_runtime_before_provisional_proof() {
        for shell in ["sh", "bash"] {
            let directory = runtime_directory();
            let bin = directory.join(".local/bin");
            fs::create_dir_all(&bin).unwrap();
            let destination = bin.join("boomux");
            fs::write(&destination, b"previous").unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
            let replacement = String::from(
                "#!/bin/sh\ntransaction=\"$HOME/.local/bin/$2\"\nprintf '%s' \"$XDG_RUNTIME_DIR\" > \"$HOME/observed-runtime\"\n/bin/mv \"$transaction/new\" \"$HOME/.local/bin/boomux\"\n: > \"$transaction/activated\"\n",
            );
            let transaction = run_local_upload_with_command(
                &directory,
                replacement.as_bytes(),
                shell,
                REMOTE_INSTALL_COMMAND,
            );
            let command_text = REMOTE_INSTALL_ACTIVATE_COMMAND.replacen(
                "boomux_runtime=/run/user/$boomux_uid",
                "boomux_runtime=$HOME/runtime",
                1,
            );
            assert_ne!(command_text, REMOTE_INSTALL_ACTIVATE_COMMAND);
            let proof = RemoteDaemonStatus::Present {
                protocol_version: protocol::PROTOCOL_VERSION,
                pid: Some(1),
                executable: Some(
                    RemoteExecutable::parse(destination.to_string_lossy().into_owned()).unwrap(),
                ),
                socket_device: Some(1),
                socket_inode: Some(1),
            };
            let mut activate = local_shell_command(shell, &directory);
            activate
                .env_remove("XDG_RUNTIME_DIR")
                .args(["-c", &command_text]);
            let output = run_streaming_command_capture(
                activate,
                transaction.activation_input(RemoteInstallReason::Upgrade, &proof),
                Duration::from_secs(1),
            )
            .unwrap();
            assert!(parse_install_activation(&output.stdout).unwrap());
            assert_eq!(
                fs::read_to_string(directory.join("observed-runtime")).unwrap(),
                directory.join("runtime").to_string_lossy()
            );
            assert_eq!(fs::read(&destination).unwrap(), replacement.as_bytes());
            run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
            assert_eq!(fs::read(&destination).unwrap(), b"previous");
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn failed_activation_restores_uploaded_state_for_exact_retry() {
        for shell in ["sh", "bash"] {
            let directory = runtime_directory();
            let bin = directory.join(".local/bin");
            fs::create_dir_all(&bin).unwrap();
            let destination = bin.join("boomux");
            fs::write(&destination, b"previous").unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
            let replacement = b"#!/bin/sh\ntransaction=\"$HOME/.local/bin/$2\"\n/bin/mv \"$transaction/new\" \"$HOME/.local/bin/boomux\"\n: > \"$transaction/activated\"\nif [ ! -e \"$HOME/failed-once\" ]; then : > \"$HOME/failed-once\"; exit 97; fi\n";
            let transaction = run_local_upload_with_command(
                &directory,
                replacement,
                shell,
                REMOTE_INSTALL_COMMAND,
            );
            let proof = RemoteDaemonStatus::Present {
                protocol_version: protocol::PROTOCOL_VERSION,
                pid: Some(1),
                executable: Some(
                    RemoteExecutable::parse(destination.to_string_lossy().into_owned()).unwrap(),
                ),
                socket_device: Some(1),
                socket_inode: Some(1),
            };
            let activate_once = || {
                let mut activate = local_shell_command(shell, &directory);
                activate.args(["-c", REMOTE_INSTALL_ACTIVATE_COMMAND]);
                run_streaming_command_capture(
                    activate,
                    transaction.activation_input(RemoteInstallReason::Upgrade, &proof),
                    Duration::from_secs(1),
                )
            };
            assert!(activate_once().is_err());
            let transaction_dir = bin.join(&transaction.0);
            assert_eq!(fs::read(&destination).unwrap(), b"previous");
            assert_eq!(fs::read(transaction_dir.join("new")).unwrap(), replacement);
            for marker in ["activated", "restore_required", "backup_ready", "missing"] {
                assert!(!transaction_dir.join(marker).exists(), "stale {marker}");
            }
            let output = activate_once().unwrap();
            assert!(parse_install_activation(&output.stdout).unwrap());
            assert_eq!(fs::read(&destination).unwrap(), replacement);
            run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
            assert_eq!(fs::read(&destination).unwrap(), b"previous");
            assert!(!transaction_dir.exists());
            assert!(!bin.join(".boomux.bootstrap.lock").exists());
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn activation_retry_finalizes_durable_activation_when_compensation_cannot_move_it() {
        let directory = runtime_directory();
        let bin = directory.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let destination = bin.join("boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let replacement = b"#!/bin/sh\ntransaction=\"$HOME/.local/bin/$2\"\n/bin/mv -f \"$transaction/new\" \"$HOME/.local/bin/boomux\"\n: > \"$transaction/activated\"\nexit 97\n";
        let transaction =
            run_local_upload_with_command(&directory, replacement, "sh", REMOTE_INSTALL_COMMAND);
        let proof = RemoteDaemonStatus::Present {
            protocol_version: protocol::PROTOCOL_VERSION,
            pid: Some(1),
            executable: Some(
                RemoteExecutable::parse(destination.to_string_lossy().into_owned()).unwrap(),
            ),
            socket_device: Some(1),
            socket_inode: Some(1),
        };
        let interrupted = REMOTE_INSTALL_ACTIVATE_COMMAND.replace(
            "/bin/mv -f \"$destination\" \"$transaction/new\" || return 1;",
            "false || return 1;",
        );
        let mut first = local_shell_command("sh", &directory);
        first.args(["-c", &interrupted]);
        assert!(
            run_streaming_command_capture(
                first,
                transaction.activation_input(RemoteInstallReason::Upgrade, &proof),
                Duration::from_secs(1),
            )
            .is_err()
        );
        let transaction_dir = bin.join(&transaction.0);
        assert!(transaction_dir.join("activated").exists());
        assert!(transaction_dir.join("restore_required").exists());
        assert!(!transaction_dir.join("new").exists());
        assert_eq!(fs::read(&destination).unwrap(), replacement);

        let mut retry = local_shell_command("sh", &directory);
        retry.args(["-c", REMOTE_INSTALL_ACTIVATE_COMMAND]);
        let output = run_streaming_command_capture(
            retry,
            transaction.activation_input(RemoteInstallReason::Upgrade, &proof),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(parse_install_activation(&output.stdout).unwrap());
        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn incomplete_activation_compensation_retains_recovery_markers() {
        let directory = runtime_directory();
        let bin = directory.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let destination = bin.join("boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let replacement = b"#!/bin/sh\ntransaction=\"$HOME/.local/bin/$2\"\n/bin/mv \"$transaction/new\" \"$HOME/.local/bin/boomux\"\n: > \"$transaction/activated\"\n/bin/mkdir \"$transaction/new\"\nexit 97\n";
        let transaction =
            run_local_upload_with_command(&directory, replacement, "sh", REMOTE_INSTALL_COMMAND);
        let proof = RemoteDaemonStatus::Present {
            protocol_version: protocol::PROTOCOL_VERSION,
            pid: Some(1),
            executable: Some(
                RemoteExecutable::parse(destination.to_string_lossy().into_owned()).unwrap(),
            ),
            socket_device: Some(1),
            socket_inode: Some(1),
        };
        let mut activate = local_shell_command("sh", &directory);
        activate.args(["-c", REMOTE_INSTALL_ACTIVATE_COMMAND]);
        assert!(
            run_streaming_command_capture(
                activate,
                transaction.activation_input(RemoteInstallReason::Upgrade, &proof),
                Duration::from_secs(1),
            )
            .is_err()
        );
        let transaction_dir = bin.join(&transaction.0);
        for marker in ["activated", "restore_required", "backup_ready"] {
            assert!(transaction_dir.join(marker).exists(), "missing {marker}");
        }
        assert!(transaction_dir.join("backup").exists());
        assert!(transaction_dir.join("new").is_dir());
        assert_eq!(fs::read(&destination).unwrap(), replacement);

        let mut retry = local_shell_command("sh", &directory);
        retry.args(["-c", REMOTE_INSTALL_ACTIVATE_COMMAND]);
        assert!(
            run_streaming_command_capture(
                retry,
                transaction.activation_input(RemoteInstallReason::Upgrade, &proof),
                Duration::from_secs(1),
            )
            .is_err(),
            "retry must not accept an activated marker while compensation is incomplete"
        );

        fs::remove_dir(transaction_dir.join("new")).unwrap();
        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        assert!(!transaction_dir.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn activation_retry_reconciles_partially_compensated_backup() {
        let directory = runtime_directory();
        let bin = directory.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let destination = bin.join("boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let replacement = b"#!/bin/sh\ntransaction=\"$HOME/.local/bin/$2\"\n/bin/mv -f \"$transaction/new\" \"$HOME/.local/bin/boomux\"\n: > \"$transaction/activated\"\n";
        let transaction =
            run_local_upload_with_command(&directory, replacement, "sh", REMOTE_INSTALL_COMMAND);
        let transaction_dir = bin.join(&transaction.0);
        fs::copy(&destination, transaction_dir.join("backup")).unwrap();
        fs::write(transaction_dir.join("backup_ready"), b"").unwrap();
        fs::write(transaction_dir.join("restore_required"), b"").unwrap();
        fs::write(
            transaction_dir.join("prior_daemon"),
            protocol::PROTOCOL_VERSION.to_string(),
        )
        .unwrap();
        fs::remove_file(&destination).unwrap();
        assert!(transaction_dir.join("new").exists());

        let proof = RemoteDaemonStatus::Present {
            protocol_version: protocol::PROTOCOL_VERSION,
            pid: Some(1),
            executable: Some(
                RemoteExecutable::parse(destination.to_string_lossy().into_owned()).unwrap(),
            ),
            socket_device: Some(1),
            socket_inode: Some(1),
        };
        let mut activate = local_shell_command("sh", &directory);
        activate.args(["-c", REMOTE_INSTALL_ACTIVATE_COMMAND]);
        let output = run_streaming_command_capture(
            activate,
            transaction.activation_input(RemoteInstallReason::Upgrade, &proof),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(parse_install_activation(&output.stdout).unwrap());
        assert_eq!(fs::read(&destination).unwrap(), replacement);
        assert!(!transaction_dir.join("missing").exists());
        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn install_waits_for_a_delayed_watchdog_readiness_marker() {
        for shell in ["sh", "bash"] {
            let directory = runtime_directory();
            let command = delay_watchdog_readiness(REMOTE_INSTALL_COMMAND);
            let transaction =
                run_local_upload_with_command(&directory, b"replacement", shell, &command);
            assert!(
                directory
                    .join(".local/bin")
                    .join(&transaction.0)
                    .join("new_ready")
                    .exists()
            );
            run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn install_rejects_symlink_special_and_non_executable_destinations_before_activation() {
        use std::os::unix::fs::FileTypeExt;

        for kind in ["symlink", "fifo", "non-executable"] {
            let directory = runtime_directory();
            let bin = directory.join(".local/bin");
            fs::create_dir_all(&bin).unwrap();
            let destination = bin.join("boomux");
            let target = directory.join("target");
            match kind {
                "symlink" => {
                    fs::write(&target, b"outside").unwrap();
                    std::os::unix::fs::symlink(&target, &destination).unwrap();
                }
                "fifo" => {
                    let path = std::ffi::CString::new(destination.as_os_str().as_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o700) }, 0);
                }
                "non-executable" => {
                    fs::write(&destination, b"old").unwrap();
                    fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).unwrap();
                }
                _ => unreachable!(),
            }

            let transaction = run_local_upload_with_command(
                &directory,
                b"replacement",
                "sh",
                REMOTE_INSTALL_COMMAND,
            );
            let result = run_activation_script(
                "sh",
                REMOTE_INSTALL_ACTIVATE_COMMAND,
                &directory,
                &transaction,
            );
            assert!(result.is_err(), "{kind}");
            match kind {
                "symlink" => {
                    assert_eq!(fs::read_link(&destination).unwrap(), target);
                    assert_eq!(fs::read(&target).unwrap(), b"outside");
                }
                "fifo" => assert!(
                    fs::symlink_metadata(&destination)
                        .unwrap()
                        .file_type()
                        .is_fifo()
                ),
                "non-executable" => {
                    assert_eq!(fs::read(&destination).unwrap(), b"old");
                    assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o777, 0o600);
                }
                _ => unreachable!(),
            }
            run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
            assert!(!bin.join(".boomux.bootstrap.lock").exists());
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn disk_full_backup_copy_failure_leaves_the_old_destination_untouched() {
        use std::os::unix::fs::FileTypeExt;

        let directory = runtime_directory();
        let bin = directory.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let destination = bin.join("boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let transaction =
            run_local_upload_with_command(&directory, b"replacement", "sh", REMOTE_INSTALL_COMMAND);
        let command = REMOTE_INSTALL_ACTIVATE_COMMAND.replacen(
            "backup=$transaction/backup;",
            "backup=/dev/full;",
            1,
        );
        assert_ne!(command, REMOTE_INSTALL_ACTIVATE_COMMAND);
        assert!(run_activation_script("sh", &command, &directory, &transaction).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o777, 0o755);
        assert!(
            fs::metadata("/dev/full")
                .unwrap()
                .file_type()
                .is_char_device()
        );
        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
        assert!(!bin.join(".boomux.bootstrap.lock").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    fn run_install_script_raw(
        shell: &str,
        command_text: &str,
        home: &Path,
        input: &[u8],
    ) -> std::process::Output {
        let transaction = InstallTransactionId::generate();
        let shell = match shell {
            "sh" => "/bin/sh",
            "bash" => "/bin/bash",
            _ => panic!("unsupported test shell: {shell}"),
        };
        let mut command = Command::new(shell);
        command
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .args(["-c", command_text])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if home.is_absolute() && home.is_dir() {
            command.current_dir(home);
        }
        let mut child = command.spawn().unwrap();
        let _ = child
            .stdin
            .take()
            .unwrap()
            .write_all(&transaction.upload_input(input));
        child.wait_with_output().unwrap()
    }

    fn run_activation_script(
        shell: &str,
        command_text: &str,
        home: &Path,
        transaction: &InstallTransactionId,
    ) -> io::Result<()> {
        let mut child = local_shell_command(shell, home);
        child.args(["-c", command_text]);
        run_streaming_command(
            child,
            transaction.activation_input(
                RemoteInstallReason::Missing,
                &RemoteDaemonStatus::Present {
                    protocol_version: protocol::PROTOCOL_VERSION,
                    pid: Some(1),
                    executable: Some(RemoteExecutable::parse("/tmp/test").unwrap()),
                    socket_device: Some(1),
                    socket_inode: Some(1),
                },
            ),
            Duration::from_secs(1),
        )
    }

    #[test]
    fn every_install_stage_reports_only_a_bounded_marker_and_rolls_back() {
        let stages = [
            ("directory", 75),
            ("lock", 76),
            ("transaction", 77),
            ("lock_id", 79),
            ("stream", 80),
            ("mode", 81),
            ("watchdog_spawn", 84),
            ("watchdog_ready", 85),
            ("watchdog_pid", 86),
            ("result", 87),
        ];
        for shell in ["sh", "bash"] {
            for (stage, code) in stages {
                let directory = runtime_directory();
                let bin = directory.join(".local/bin");
                fs::create_dir_all(&bin).unwrap();
                let destination = bin.join("boomux");
                fs::write(&destination, b"previous").unwrap();
                fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
                let assignment = format!("stage={stage}; stage_code={code};");
                let injected = format!("{assignment} printf 'private remote detail' >&2; false;");
                let command = REMOTE_INSTALL_COMMAND.replacen(&assignment, &injected, 1);
                assert_ne!(command, REMOTE_INSTALL_COMMAND);

                let output = run_install_script_raw(shell, &command, &directory, b"replacement");
                assert_eq!(output.status.code(), Some(code), "{shell} stage {stage}");
                assert_eq!(
                    output.stderr,
                    format!("{INSTALL_STAGE_PREFIX}:{stage}:{code}\n").as_bytes(),
                    "{shell} stage {stage}"
                );
                let error = classify_ssh_command_failure(output.status.code(), &output.stderr);
                assert_eq!(error_code(&error), "bootstrap_install_failed");
                assert!(error.to_string().contains("remote Boomux install failed"));
                assert!(!error.to_string().contains("private remote detail"));
                assert_eq!(fs::read(&destination).unwrap(), b"previous");
                assert!(
                    fs::read_dir(&bin)
                        .unwrap()
                        .filter_map(Result::ok)
                        .all(|entry| !entry.file_name().to_string_lossy().starts_with(".boomux.")),
                    "{shell} stage {stage} left transaction state"
                );
                fs::remove_dir_all(directory).unwrap();
            }
        }
    }

    #[test]
    fn invalid_remote_home_has_a_fixed_non_secret_install_stage() {
        let output = run_install_script_raw(
            "bash",
            REMOTE_INSTALL_COMMAND,
            Path::new("relative-home"),
            b"replacement",
        );
        assert_eq!(output.status.code(), Some(74));
        assert_eq!(output.stderr, b"boomux-install-stage-v1:home:74\n");
        let error = classify_ssh_command_failure(output.status.code(), &output.stderr);
        assert_eq!(error_code(&error), "bootstrap_install_failed");
        assert!(error.to_string().contains("HOME is not an absolute path"));
    }

    #[test]
    fn failed_watchdog_readiness_is_typed_and_restores_the_previous_binary() {
        let directory = runtime_directory();
        let bin = directory.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let destination = bin.join("boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let command = REMOTE_INSTALL_COMMAND.replacen(
            ": > \"$transaction/watchdog_ready\"; claim_process_start",
            "exit 1; claim_process_start",
            1,
        );
        let output = run_install_script_raw("bash", &command, &directory, b"replacement");
        assert_eq!(output.status.code(), Some(85));
        assert_eq!(
            output.stderr,
            b"boomux-install-stage-v1:watchdog_ready:85\n"
        );
        let error = classify_ssh_command_failure(output.status.code(), &output.stderr);
        assert_eq!(error_code(&error), "bootstrap_install_failed");
        assert!(error.to_string().contains("watchdog did not become ready"));
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        assert!(!bin.join(".boomux.bootstrap.lock").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    fn run_local_transaction(home: &Path, command: &str, transaction: &InstallTransactionId) {
        let mut child = local_shell_command("sh", home);
        child.args(["-c", command]);
        run_streaming_command(child, transaction.input(), Duration::from_secs(1)).unwrap();
    }

    fn compatible_helper_script(node_id: &str) -> String {
        let handshake = FederationHandshake {
            version: FEDERATION_VERSION,
            node_id: node_id.into(),
            helper_version: env!("CARGO_PKG_VERSION").into(),
            core_protocol_version: protocol::PROTOCOL_VERSION,
            connection_mode: FederationConnectionMode::AdHoc,
        };
        let mut handshake_bytes = Vec::new();
        crate::federation::write_handshake(&mut handshake_bytes, &handshake).unwrap();
        let mut request_bytes = Vec::new();
        protocol::write_message(
            &mut request_bytes,
            &Envelope::with_version(protocol::PROTOCOL_VERSION, Request::Ping),
        )
        .unwrap();
        let mut response_bytes = Vec::new();
        protocol::write_message(
            &mut response_bytes,
            &Envelope::with_version(protocol::PROTOCOL_VERSION, Response::Pong),
        )
        .unwrap();
        format!(
            "{}; dd bs=1 count={} of=/dev/null 2>/dev/null; {}",
            shell_printf(&handshake_bytes),
            request_bytes.len(),
            shell_printf(&response_bytes),
        )
    }

    fn write_bootstrap_ssh(runtime: &Path, executable_cases: &str, candidates: &str) -> PathBuf {
        fs::create_dir_all(runtime).unwrap();
        let ssh = runtime.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in *'exec '*) last=${{last##*exec }} ;; esac\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0{candidates}' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/person/.local/bin/boomux\\0clear\\0' ;;\n{executable_cases}\n  *) exit 64 ;;\nesac\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        ssh
    }

    fn write_session_bootstrap_ssh(
        runtime: &Path,
        executable_cases: &str,
        candidates: &str,
    ) -> PathBuf {
        fs::create_dir_all(runtime).unwrap();
        let ssh = runtime.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\n{CONTROL_MASTER_SCRIPT}\ncase \"$last\" in *'exec '*) last=${{last##*exec }} ;; esac\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0{candidates}' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/person/.local/bin/boomux\\0clear\\0' ;;\n{executable_cases}\n  *) exit 64 ;;\nesac\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        ssh
    }

    #[test]
    fn rejects_option_like_and_unbounded_targets() {
        for target in ["", "-oProxyCommand=bad", "host name", "host\nname"] {
            assert_eq!(
                SshTarget::parse(target).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(
            SshTarget::parse("x".repeat(MAX_SSH_TARGET_BYTES + 1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            SshTarget::parse("user@workbox").unwrap().as_str(),
            "user@workbox"
        );

        for executable in ["boomux", "relative/boomux", "/opt/boomux\nbin"] {
            assert_eq!(
                RemoteExecutable::parse(executable).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn builds_exact_fixed_ssh_arguments_and_quotes_only_the_executable() {
        let runtime = runtime_directory();
        let invocation = SshInvocation::prepare_at(
            &runtime,
            Some(Path::new("/home/person/.ssh/config")),
            SshTarget::parse("workbox").unwrap(),
            RemoteExecutable::parse("/opt/boomux's bin/boomux").unwrap(),
            SshAuthenticationMode::Interactive,
        )
        .unwrap();
        let command = invocation.command();
        assert_eq!(command.get_program(), "ssh");
        let arguments = command_arguments(&command);
        assert_eq!(arguments[0], "-F");
        assert_eq!(arguments[1], invocation.config_path().as_os_str());
        assert_eq!(arguments[2], "-T");
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-o", "ClearAllForwardings=yes"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-o", "BatchMode=no"])
        );
        assert_eq!(arguments[arguments.len() - 2], "workbox");
        assert_eq!(
            arguments.last().unwrap(),
            &OsString::from(remote_helper_command(
                &RemoteExecutable::parse("/opt/boomux's bin/boomux").unwrap()
            ))
        );
        assert!(
            arguments
                .last()
                .unwrap()
                .to_string_lossy()
                .contains("exec '/opt/boomux'\\''s bin/boomux' __federation-stdio")
        );

        let config = fs::read_to_string(invocation.config_path()).unwrap();
        assert!(
            config.starts_with(
                "Include \"/home/person/.ssh/config\"\nMatch all\nSendEnv -*\nHost *\n"
            )
        );
        assert!(config.contains("ServerAliveInterval 15"));
        assert_eq!(
            fs::symlink_metadata(invocation.config_path())
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        let directory = invocation.directory.clone();
        drop(invocation);
        assert!(!directory.exists());
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn generated_config_ends_trailing_included_match_before_clearing_sendenv() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let user_config = runtime.join("user-config");
        fs::write(
            &user_config,
            "Host *\n    SendEnv BOOMUX_SECRET\nMatch host never-matches\n",
        )
        .unwrap();
        let invocation = SshInvocation::prepare_at(
            &runtime,
            Some(&user_config),
            SshTarget::parse("workbox").unwrap(),
            RemoteExecutable::parse("/usr/bin/boomux").unwrap(),
            SshAuthenticationMode::Batch,
        )
        .unwrap();
        let output = Command::new("ssh")
            .args(["-G", "-F"])
            .arg(invocation.config_path())
            .arg("workbox")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ssh -G failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let effective = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
        assert!(!effective.contains("boomux_secret"), "{effective}");
        assert!(!effective.lines().any(|line| line.starts_with("sendenv ")));
        drop(invocation);
        fs::remove_dir_all(runtime).unwrap();
    }

    fn run_runtime_prefix(runtime: Option<&Path>, path: Option<&Path>) -> std::process::Output {
        run_runtime_prefix_text(REMOTE_RUNTIME_PREFIX, runtime, path)
    }

    fn run_runtime_prefix_text(
        prefix: &str,
        runtime: Option<&Path>,
        path: Option<&Path>,
    ) -> std::process::Output {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &format!("{prefix}printf '%s' \"$XDG_RUNTIME_DIR\"")])
            .env_remove("XDG_RUNTIME_DIR");
        if let Some(runtime) = runtime {
            command.env("XDG_RUNTIME_DIR", runtime);
        }
        if let Some(path) = path {
            command.env("PATH", path);
        }
        command.output().unwrap()
    }

    #[test]
    fn linux_runtime_discovery_derives_or_validates_owner_runtime_without_local_forwarding() {
        let uid = unsafe { libc::geteuid() };
        let derived = PathBuf::from(format!("/run/user/{uid}"));
        let output = run_runtime_prefix(None, None);
        assert!(
            output.status.success(),
            "runtime discovery failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, derived.as_os_str().as_bytes());

        let supplied = runtime_directory();
        fs::create_dir(&supplied).unwrap();
        fs::set_permissions(&supplied, fs::Permissions::from_mode(0o700)).unwrap();
        let output = run_runtime_prefix(Some(&supplied), None);
        assert!(output.status.success());
        assert_eq!(output.stdout, supplied.as_os_str().as_bytes());
        assert!(
            !remote_helper_command(&RemoteExecutable::parse("/bin/boomux").unwrap())
                .contains(supplied.to_str().unwrap())
        );
        fs::remove_dir_all(supplied).unwrap();
    }

    #[test]
    fn runtime_discovery_rejects_malicious_identity_and_unsafe_environment() {
        let relative = run_runtime_prefix(Some(Path::new("relative")), None);
        assert_eq!(relative.status.code(), Some(89));
        assert_eq!(relative.stderr, b"boomux-runtime-v1:invalid:89\n");
        assert_eq!(
            error_code(&classify_ssh_command_failure(
                relative.status.code(),
                &relative.stderr
            )),
            "bootstrap_runtime_unavailable"
        );

        let unsafe_runtime = runtime_directory();
        fs::create_dir(&unsafe_runtime).unwrap();
        fs::set_permissions(&unsafe_runtime, fs::Permissions::from_mode(0o755)).unwrap();
        let unsafe_output = run_runtime_prefix(Some(&unsafe_runtime), None);
        assert_eq!(unsafe_output.status.code(), Some(90));
        assert_eq!(unsafe_output.stderr, b"boomux-runtime-v1:unsafe:90\n");
        fs::remove_dir_all(unsafe_runtime).unwrap();

        let malicious_prefix =
            REMOTE_RUNTIME_PREFIX.replace("/usr/bin/id -u", "printf '1;touch-pwned\\n'");
        let malicious = run_runtime_prefix_text(&malicious_prefix, None, None);
        assert_eq!(malicious.status.code(), Some(89));
        assert_eq!(malicious.stderr, b"boomux-runtime-v1:invalid:89\n");
    }

    #[test]
    fn macos_runtime_discovery_preserves_explicit_runtime_and_rejects_absence() {
        let root = runtime_directory();
        let runtime = root.join("runtime");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let uid = unsafe { libc::geteuid() };
        let macos_prefix = REMOTE_RUNTIME_PREFIX
            .replace("/usr/bin/uname -s", "printf 'Darwin\\n'")
            .replace("/usr/bin/stat -f '%u'", &format!("printf '{uid}\\n'"))
            .replace("/usr/bin/stat -f '%Lp'", "printf '700\\n'");
        let supplied = run_runtime_prefix_text(&macos_prefix, Some(&runtime), None);
        assert!(supplied.status.success());
        assert_eq!(supplied.stdout, runtime.as_os_str().as_bytes());

        let missing = run_runtime_prefix_text(&macos_prefix, None, None);
        assert_eq!(missing.status.code(), Some(88));
        assert_eq!(missing.stderr, b"boomux-runtime-v1:missing:88\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn post_install_failures_name_the_fixed_stage_without_remote_stderr() {
        for (stage, expected) in [
            ("daemon_status", "daemon status"),
            ("daemon_restart", "daemon restart"),
            ("helper_verification", "helper verification"),
            ("live_handshake", "live helper handshake"),
            ("protocol_ping", "protocol ping"),
        ] {
            let error =
                post_install_failure(stage, io::Error::other("private remote stderr and path"));
            assert_eq!(error_code(&error), "bootstrap_install_failed");
            assert!(error.to_string().contains(expected));
            assert!(!error.to_string().contains("private remote"));
        }
        let runtime = classify_ssh_command_failure(
            Some(90),
            b"boomux-runtime-v1:unsafe:90\nprivate remote stderr",
        );
        let preserved = post_install_failure("daemon_status", runtime);
        assert_eq!(error_code(&preserved), "bootstrap_runtime_unavailable");
        assert!(!preserved.to_string().contains("private remote"));
    }

    #[test]
    fn daemon_status_distinguishes_absent_compatible_and_incompatible() {
        assert_eq!(
            parse_remote_daemon_status(b"boomux-daemon-status-v1\0absent\0").unwrap(),
            RemoteDaemonStatus::Absent
        );
        for (version, expected) in [
            (
                21,
                RemoteDaemonStatus::Present {
                    protocol_version: 21,
                    pid: None,
                    executable: None,
                    socket_device: None,
                    socket_inode: None,
                },
            ),
            (
                protocol::PROTOCOL_VERSION,
                RemoteDaemonStatus::Present {
                    protocol_version: protocol::PROTOCOL_VERSION,
                    pid: None,
                    executable: None,
                    socket_device: None,
                    socket_inode: None,
                },
            ),
            (
                protocol::PROTOCOL_VERSION + 1,
                RemoteDaemonStatus::Present {
                    protocol_version: protocol::PROTOCOL_VERSION + 1,
                    pid: None,
                    executable: None,
                    socket_device: None,
                    socket_inode: None,
                },
            ),
        ] {
            let status = format!(
                "{{\"schema\":\"boomux.cli/v1\",\"command\":\"daemon.status\",\"data\":{{\"protocol_version\":{version}}}}}"
            );
            assert_eq!(
                parse_remote_daemon_status(status.as_bytes()).unwrap(),
                expected
            );
        }
        let status = br#"{"schema":"boomux.cli/v1","command":"daemon.status","data":{"protocol_version":38,"pid":42,"executable":"/custom/path with spaces/boomux","socket_device":7,"socket_inode":8}}"#;
        let parsed = parse_remote_daemon_status(status).unwrap();
        assert!(parsed.proves_executable(
            &RemoteExecutable::parse("/custom/path with spaces/boomux").unwrap()
        ));
        assert!(!parsed.proves_executable(&RemoteExecutable::parse("/other/boomux").unwrap()));

        let runtime = runtime_directory();
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = Command::new("sh");
        command
            .args(["-c", &remote_daemon_status_command()])
            .env("HOME", &runtime)
            .env("XDG_RUNTIME_DIR", &runtime);
        let output = run_bounded_command(command, Duration::from_secs(1)).unwrap();
        assert_eq!(
            parse_remote_daemon_status(&output.stdout).unwrap(),
            RemoteDaemonStatus::Absent
        );
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn explicit_upgrade_restart_policy_forces_only_present_compatible_daemons() {
        let compatible = RemoteDaemonStatus::Present {
            protocol_version: protocol::PROTOCOL_VERSION,
            pid: None,
            executable: None,
            socket_device: None,
            socket_inode: None,
        };
        let automatic = RemoteInstallIntent::AutomaticCompatibility;
        let explicit = RemoteInstallIntent::ExplicitRegisteredUpgrade {
            expected_node_id: Uuid::new_v4().to_string(),
        };

        assert!(!automatic.daemon_restart_required(&compatible));
        assert!(explicit.daemon_restart_required(&compatible));
        assert!(!explicit.daemon_restart_required(&RemoteDaemonStatus::Absent));
    }

    #[test]
    fn running_non_destination_helper_rejects_shadow_upgrade_before_remote_mutation() {
        for (helper_path, daemon_executable) in [
            ("/usr/bin/boomux", "/usr/bin/boomux"),
            ("/custom/bin/boomux", "/custom/bin/boomux"),
            (
                "/custom/path with spaces/boomux",
                "/custom/path with spaces/boomux",
            ),
            (
                "/home/person/.local/bin/boomux",
                "/deleted/undiscovered/boomux",
            ),
        ] {
            let runtime = runtime_directory();
            fs::create_dir_all(&runtime).unwrap();
            let mutated = runtime.join("mutated");
            let log = runtime.join("ssh.log");
            let ssh = runtime.join("ssh");
            let status = serde_json::json!({
                "schema": "boomux.cli/v1",
                "command": "daemon.status",
                "data": {
                    "protocol_version": 21,
                    "pid": 123,
                    "executable": daemon_executable,
                }
            })
            .to_string();
            fs::write(
                &ssh,
                format!(
                    "#!/bin/sh\n{CONTROL_MASTER_SCRIPT}\nlast=\nfor arg do last=$arg; done\nprintf '%s\\n' \"$last\" >> {}\ncase \"$last\" in\n  *'daemon status --json'*) printf '%s' {} ;;\n  *'boomux-install-transaction-v1'*) cat >/dev/null; printf 'boomux-install-transaction-v1\\0.boomux.bootstrap.ABC12345\\0' ;;\n  *'transaction/watchdog_pid'*'restore_install'*) cat >/dev/null ;;\n  *'daemon restart'*|*'daemon stop'*) : > {}; exit 99 ;;\n  *) exit 64 ;;\nesac\n",
                    quote_posix_shell(log.to_str().unwrap()),
                    quote_posix_shell(&status),
                    quote_posix_shell(mutated.to_str().unwrap()),
                ),
            )
            .unwrap();
            fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
            let session = BootstrapSession::open_at(
                &runtime,
                None,
                SshTarget::parse("workbox").unwrap(),
                SshAuthenticationMode::Batch,
                Duration::from_secs(1),
                ssh.as_os_str(),
            )
            .unwrap();
            let plan = RemoteInstallPlan {
                target: SshTarget::parse("workbox").unwrap(),
                destination: RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
                source: RemoteInstallSource::CurrentBinary {
                    path: runtime.join("pinned"),
                    sha256: format!("{:x}", Sha256::digest(b"replacement")),
                    bytes: b"replacement".to_vec(),
                },
                reason: RemoteInstallReason::Upgrade,
                bootstrap_id: Some(session.id),
                upgrade_helper: Some(RemoteExecutable::parse(helper_path).unwrap()),
                intent: RemoteInstallIntent::AutomaticCompatibility,
            };
            let error = session
                .install_and_connect(&plan, Duration::from_secs(1))
                .err()
                .expect("shadow upgrade must fail");
            assert_eq!(error_code(&error), "upgrade_required");
            assert_eq!(
                recovery_disposition(&error),
                BootstrapRecoveryDisposition::RollbackConfirmed
            );
            assert!(error.to_string().contains(helper_path));
            assert!(error.to_string().contains("owner or package mechanism"));
            assert!(error.to_string().contains("explicitly stop the daemon"));
            assert!(!mutated.exists());
            let commands = fs::read_to_string(&log).unwrap();
            assert!(commands.contains("/new\" daemon status --json"));
            assert!(commands.contains("boomux-install-transaction-v1"));
            fs::remove_dir_all(runtime).unwrap();
        }
    }

    #[test]
    fn missing_helper_requires_proven_socket_absence_before_remote_mutation() {
        for (case, probe) in [
            (
                "undiscovered daemon",
                "printf 'boomux-daemon-presence-v1\\0present\\0'",
            ),
            (
                "stale socket",
                "printf 'boomux-daemon-presence-v1\\0present\\0'",
            ),
            ("unknown presence", "exit 71"),
        ] {
            let runtime = runtime_directory();
            fs::create_dir_all(&runtime).unwrap();
            let mutated = runtime.join("mutated");
            let ssh = runtime.join("ssh");
            fs::write(
                &ssh,
                format!(
                    "#!/bin/sh\n{CONTROL_MASTER_SCRIPT}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *'boomux-daemon-presence-v1'*) {probe} ;;\n  *'boomux-install-transaction-v1'*|*'daemon restart'*|*'daemon stop'*) : > {}; exit 99 ;;\n  *) exit 64 ;;\nesac\n",
                    quote_posix_shell(mutated.to_str().unwrap()),
                ),
            )
            .unwrap();
            fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
            let session = BootstrapSession::open_at(
                &runtime,
                None,
                SshTarget::parse("workbox").unwrap(),
                SshAuthenticationMode::Batch,
                Duration::from_secs(1),
                ssh.as_os_str(),
            )
            .unwrap();
            let plan = RemoteInstallPlan {
                target: SshTarget::parse("workbox").unwrap(),
                destination: RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
                source: RemoteInstallSource::CurrentBinary {
                    path: runtime.join("pinned"),
                    sha256: format!("{:x}", Sha256::digest(b"replacement")),
                    bytes: b"replacement".to_vec(),
                },
                reason: RemoteInstallReason::Missing,
                bootstrap_id: Some(session.id),
                upgrade_helper: None,
                intent: RemoteInstallIntent::AutomaticCompatibility,
            };
            let error = session
                .install_and_connect(&plan, Duration::from_secs(1))
                .err()
                .unwrap_or_else(|| panic!("{case} must prevent installation"));
            assert_eq!(error_code(&error), "install_required", "{case}");
            assert!(!mutated.exists(), "{case}");
            fs::remove_dir_all(runtime).unwrap();
        }
    }

    #[test]
    fn live_helper_handshake_preserves_runtime_discovery_failure() {
        let unsafe_runtime = runtime_directory();
        fs::create_dir(&unsafe_runtime).unwrap();
        fs::set_permissions(&unsafe_runtime, fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = Command::new("sh");
        command
            .args(["-c", &format!("{REMOTE_RUNTIME_PREFIX}exec false")])
            .env("XDG_RUNTIME_DIR", &unsafe_runtime);
        let helper = CompatibleRemoteHelper {
            executable: RemoteExecutable::parse("/bin/false").unwrap(),
            handshake: FederationHandshake {
                version: FEDERATION_VERSION,
                node_id: Uuid::new_v4().to_string(),
                helper_version: "test".into(),
                core_protocol_version: protocol::PROTOCOL_VERSION,
                connection_mode: FederationConnectionMode::AdHoc,
            },
            bootstrap_id: None,
        };
        let error = connect_remote_command(command, helper, Duration::from_secs(1), None)
            .err()
            .expect("unsafe runtime must fail before the helper handshake");
        assert_eq!(error_code(&error), "bootstrap_runtime_unavailable");
        assert!(error.to_string().contains("owner-controlled"));
        fs::remove_dir_all(unsafe_runtime).unwrap();
    }

    #[test]
    fn batch_mode_changes_only_the_authentication_policy() {
        let runtime = runtime_directory();
        let invocation = SshInvocation::prepare_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            RemoteExecutable::parse("/usr/bin/boomux").unwrap(),
            SshAuthenticationMode::Batch,
        )
        .unwrap();
        let arguments = command_arguments(&invocation.command());
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-o", "BatchMode=yes"])
        );
        drop(invocation);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn master_stderr_is_mirrored_live_only_for_interactive_authentication() {
        let challenge = b"To authenticate, visit:\nhttps://login.tailscale.test/a?c=123\n\xff";
        for authentication in [
            SshAuthenticationMode::Interactive,
            SshAuthenticationMode::Batch,
        ] {
            let runtime = runtime_directory();
            fs::create_dir_all(&runtime).unwrap();
            let first = shell_printf(&challenge[..17]);
            let second = shell_printf(&challenge[17..43]);
            let third = shell_printf(&challenge[43..]);
            let ssh = write_master_stderr_ssh(
                &runtime,
                &format!(
                    "{first} >&2; {second} >&2; {third} >&2; : > \"$control.ready\"; trap 'rm -f \"$control.ready\"' EXIT HUP INT TERM; while :; do sleep 60; done"
                ),
            );
            let (mut mirrored, mirror) = std::os::unix::net::UnixStream::pair().unwrap();
            mirrored
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let session = BootstrapSession::open_at_with_mirror(
                &runtime,
                None,
                SshTarget::parse("workbox").unwrap(),
                authentication,
                Duration::from_secs(2),
                ssh.as_os_str(),
                (Some(Box::new(mirror)), None),
            )
            .unwrap();

            if authentication == SshAuthenticationMode::Interactive {
                let mut output = vec![0; challenge.len()];
                mirrored.read_exact(&mut output).unwrap();
                assert_eq!(output, challenge);
            } else {
                let mut byte = [0];
                assert_eq!(mirrored.read(&mut byte).unwrap(), 0);
            }
            drop(session);
            fs::remove_dir_all(runtime).unwrap();
        }
    }

    #[test]
    fn master_stderr_classification_uses_retained_bytes_without_batch_mirroring() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let private =
            b"private challenge: https://login.test/token\nPermission denied (publickey).\n";
        let first = shell_printf(&private[..31]);
        let second = shell_printf(&private[31..]);
        let ssh =
            write_master_stderr_ssh(&runtime, &format!("{first} >&2; {second} >&2; exit 255"));
        let mirror_path = runtime.join("batch-mirror");
        let mirror = fs::File::create(&mirror_path).unwrap();
        let error = BootstrapSession::open_at_with_mirror(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(2),
            ssh.as_os_str(),
            (Some(Box::new(mirror)), None),
        )
        .err()
        .expect("the fake master must fail authentication");
        assert_eq!(error_code(&error), "bootstrap_authentication_failed");
        assert!(!error.to_string().contains("login.test"));
        assert!(fs::read(&mirror_path).unwrap().is_empty());
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn master_stderr_truncation_kills_the_waiting_master_and_reports_the_bound() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let oversized = vec![b'x'; MAX_PROBE_STDERR_BYTES + 1];
        let output = shell_printf(&oversized);
        let ssh = write_master_stderr_ssh(
            &runtime,
            &format!("{output} >&2; while :; do /bin/sleep 60; done"),
        );
        let mirror_path = runtime.join("interactive-mirror");
        let mirror = fs::File::create(&mirror_path).unwrap();
        let started = Instant::now();
        let error = BootstrapSession::open_at_with_mirror(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Interactive,
            Duration::from_secs(10),
            ssh.as_os_str(),
            (Some(Box::new(mirror)), None),
        )
        .err()
        .expect("oversized master stderr must fail");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(error_code(&error), "bootstrap_transport_failed");
        assert!(error.to_string().contains("exceeded the supported bound"));
        assert_eq!(
            fs::read(&mirror_path).unwrap(),
            &oversized[..MAX_PROBE_STDERR_BYTES]
        );
        assert!(
            fs::read_dir(&runtime)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| {
                    !entry.file_type().unwrap().is_dir()
                        || !entry.file_name().to_string_lossy().starts_with("ssh-")
                })
        );
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn real_ssh_missing_master_cannot_fall_back_to_a_network_connection() {
        let runtime = runtime_directory();
        let (directory, config, control) = prepare_ssh_directory(&runtime, None).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut file = OpenOptions::new().append(true).open(&config).unwrap();
        writeln!(
            file,
            "Host workbox\n    HostName 127.0.0.1\n    Port {port}\n    ConnectTimeout 1"
        )
        .unwrap();
        let command = slave_command(
            OsStr::new("ssh"),
            &config,
            &control,
            &SshTarget::parse("workbox").unwrap(),
            "true",
        );
        let arguments = command_arguments(&command);
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-o", "ProxyCommand=/bin/false"])
        );
        assert!(run_bounded_command(command, Duration::from_secs(2)).is_err());
        thread::sleep(Duration::from_millis(50));
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock
        ));
        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn parses_supported_platforms_and_current_release_matrix() {
        let linux = RemotePlatform::parse_probe(b"boomux-platform-v1\0Linux\0x86_64\0").unwrap();
        assert_eq!(linux.operating_system, RemoteOperatingSystem::Linux);
        assert_eq!(linux.architecture, RemoteArchitecture::X86_64);
        assert_eq!(linux.release_target(), Some("x86_64-unknown-linux-gnu"));

        let mac = RemotePlatform::parse_probe(b"boomux-platform-v1\0Darwin\0arm64\0").unwrap();
        assert_eq!(mac.operating_system, RemoteOperatingSystem::MacOs);
        assert_eq!(mac.architecture, RemoteArchitecture::Aarch64);
        assert_eq!(mac.release_target(), None);

        for invalid in [
            &b"wrong\0Linux\0x86_64\0"[..],
            &b"boomux-platform-v1\0Linux\0x86_64"[..],
        ] {
            assert_eq!(
                RemotePlatform::parse_probe(invalid).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        for unsupported in [
            &b"boomux-platform-v1\0Plan9\0x86_64\0"[..],
            &b"boomux-platform-v1\0Linux\0mips\0"[..],
        ] {
            let error = RemotePlatform::parse_probe(unsupported).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert_eq!(error_code(&error), "bootstrap_unsupported_platform");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn platform_preflight_ignores_a_poisoned_path() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", PLATFORM_PROBE_COMMAND])
            .env("PATH", "/definitely/not/a/bootstrap/tool/path");
        let output = run_bounded_command(command, Duration::from_secs(1)).unwrap();
        let platform = RemotePlatform::parse_probe(&output.stdout).unwrap();
        assert_eq!(platform.operating_system, RemoteOperatingSystem::Linux);
    }

    #[test]
    fn parses_bounded_unique_absolute_executable_candidates() {
        let candidates = parse_executable_probe(
            b"boomux-executables-v1\0/usr/bin/boomux\0/opt/boomux/bin/boomux\0/usr/bin/boomux\0",
        )
        .unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(RemoteExecutable::as_str)
                .collect::<Vec<_>>(),
            ["/usr/bin/boomux", "/opt/boomux/bin/boomux"]
        );
        for invalid in [
            b"boomux-executables-v1\0relative/boomux\0".to_vec(),
            b"boomux-executables-v1\0/opt/boomux\nbin\0".to_vec(),
            {
                let mut probe = b"boomux-executables-v1\0/".to_vec();
                probe.extend(std::iter::repeat_n(b'x', MAX_REMOTE_EXECUTABLE_BYTES));
                probe.push(0);
                probe
            },
        ] {
            let error = parse_executable_probe(&invalid).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(error_code(&error), "bootstrap_malformed_helper");
        }

        assert_eq!(
            parse_install_destination_probe(
                b"boomux-install-destination-v1\0/home/person/.local/bin/boomux\0"
            )
            .unwrap()
            .as_str(),
            "/home/person/.local/bin/boomux"
        );
        let error =
            parse_install_destination_probe(b"boomux-install-destination-v1\0relative/boomux\0")
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error_code(&error), "bootstrap_malformed_helper");
        let (destination, recovery) = parse_install_destination_state(
            b"boomux-install-destination-v1\0/home/person/.local/bin/boomux\0recovering\0",
        )
        .unwrap();
        assert_eq!(destination.as_str(), "/home/person/.local/bin/boomux");
        assert_eq!(recovery, RemoteRecoveryState::Active);
        assert_eq!(
            parse_install_destination_state(
                b"boomux-install-destination-v1\0/home/person/.local/bin/boomux\0stale\0",
            )
            .unwrap()
            .1,
            RemoteRecoveryState::Stale
        );
        for state in [b"unknown".as_slice(), b"recovering\0extra".as_slice()] {
            let mut probe =
                b"boomux-install-destination-v1\0/home/person/.local/bin/boomux\0".to_vec();
            probe.extend_from_slice(state);
            probe.push(0);
            assert!(parse_install_destination_state(&probe).is_err());
        }
    }

    #[test]
    fn probe_invocations_use_only_fixed_remote_commands() {
        let runtime = runtime_directory();
        let invocation = SshInvocation::prepare_command_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            RemoteProbe::Platform.command().to_owned(),
            SshAuthenticationMode::Interactive,
        )
        .unwrap();
        let arguments = command_arguments(&invocation.command());
        assert_eq!(arguments.last().unwrap(), PLATFORM_PROBE_COMMAND);
        drop(invocation);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn executable_probe_hides_only_an_uncommitted_provisional_destination() {
        let home = runtime_directory();
        let bin = home.join(".local/bin");
        let destination = bin.join("boomux");
        let lock = bin.join(".boomux.bootstrap.lock");
        fs::create_dir_all(&lock).unwrap();
        fs::write(&destination, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();

        let run = || {
            let mut command = Command::new("sh");
            command
                .args(["-c", EXECUTABLE_PROBE_COMMAND])
                .env("HOME", &home)
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
            let output = run_bounded_command(command, Duration::from_secs(1)).unwrap();
            parse_executable_probe(&output.stdout).unwrap()
        };
        assert!(
            !run()
                .iter()
                .any(|path| path.as_str() == destination.to_str().unwrap())
        );
        let recovery = || {
            let mut command = Command::new("sh");
            command
                .args(["-c", INSTALL_DESTINATION_PROBE_COMMAND])
                .env("HOME", &home);
            let output = run_bounded_command(command, Duration::from_secs(1)).unwrap();
            parse_install_destination_state(&output.stdout).unwrap().1
        };
        assert_eq!(recovery(), RemoteRecoveryState::Stale);
        let transaction = ".boomux.bootstrap.ABC12345";
        fs::write(lock.join("id"), format!("{transaction}\n")).unwrap();
        let transaction_dir = bin.join(transaction);
        fs::create_dir(&transaction_dir).unwrap();
        fs::write(
            transaction_dir.join("watchdog_pid"),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        #[cfg(target_os = "linux")]
        let watchdog_start = {
            let stat = fs::read_to_string(format!("/proc/{}/stat", std::process::id())).unwrap();
            stat.rsplit_once(") ")
                .unwrap()
                .1
                .split_whitespace()
                .nth(19)
                .unwrap()
                .to_owned()
        };
        #[cfg(target_os = "macos")]
        let watchdog_start = {
            let output = Command::new("ps")
                .args([
                    "-p",
                    &std::process::id().to_string(),
                    "-o",
                    "state=",
                    "-o",
                    "lstart=",
                ])
                .output()
                .unwrap();
            String::from_utf8(output.stdout)
                .unwrap()
                .split_whitespace()
                .skip(1)
                .collect::<Vec<_>>()
                .join(" ")
        };
        fs::write(
            transaction_dir.join("watchdog_start"),
            format!("{watchdog_start}\n"),
        )
        .unwrap();
        assert_eq!(recovery(), RemoteRecoveryState::Active);
        #[cfg(target_os = "linux")]
        {
            let mut zombie = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
            let zombie_pid = zombie.id();
            let deadline = Instant::now() + Duration::from_secs(1);
            let zombie_start = loop {
                let stat = fs::read_to_string(format!("/proc/{zombie_pid}/stat")).unwrap();
                let mut fields = stat.rsplit_once(") ").unwrap().1.split_whitespace();
                if fields.next().unwrap() == "Z" {
                    break fields.nth(18).unwrap().to_owned();
                }
                assert!(Instant::now() < deadline, "child did not become a zombie");
                thread::sleep(Duration::from_millis(10));
            };
            fs::write(
                transaction_dir.join("watchdog_pid"),
                format!("{zombie_pid}\n"),
            )
            .unwrap();
            fs::write(
                transaction_dir.join("watchdog_start"),
                format!("{zombie_start}\n"),
            )
            .unwrap();
            assert_eq!(recovery(), RemoteRecoveryState::Stale);
            zombie.wait().unwrap();
        }

        fs::create_dir(lock.join("committed")).unwrap();
        assert_eq!(recovery(), RemoteRecoveryState::Clear);
        assert!(
            run()
                .iter()
                .any(|path| path.as_str() == destination.to_str().unwrap())
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn bounded_runner_captures_output_and_classifies_exit() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf stdout; printf stderr >&2"]);
        let output = run_bounded_command(command, Duration::from_secs(1)).unwrap();
        assert_eq!(output.stdout, b"stdout");
        assert_eq!(output.stderr, b"stderr");

        let mut command = Command::new("sh");
        command.args(["-c", "exit 7"]);
        let error = run_bounded_command(command, Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "SSH remote command failed; verify executable access and remote shell support"
        );
    }

    #[test]
    fn bounded_runner_kills_process_group_on_timeout() {
        let started = Instant::now();
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        assert_eq!(
            run_bounded_command(command, Duration::from_millis(50))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bounded_runner_rejects_oversized_output_and_timeout() {
        let mut command = Command::new("sh");
        command.args(["-c", "head -c 70000 /dev/zero"]);
        assert_eq!(
            run_bounded_command(command, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        assert_eq!(
            run_bounded_command(Command::new("true"), Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn helper_probe_verifies_handshake_and_ping_on_one_channel() {
        let handshake = FederationHandshake {
            version: FEDERATION_VERSION,
            node_id: Uuid::new_v4().to_string(),
            helper_version: "0.18.0".into(),
            core_protocol_version: protocol::PROTOCOL_VERSION,
            connection_mode: FederationConnectionMode::AdHoc,
        };
        let mut handshake_bytes = Vec::new();
        crate::federation::write_handshake(&mut handshake_bytes, &handshake).unwrap();
        let mut request_bytes = Vec::new();
        protocol::write_message(
            &mut request_bytes,
            &Envelope::with_version(protocol::PROTOCOL_VERSION, Request::Ping),
        )
        .unwrap();
        let mut response_bytes = Vec::new();
        protocol::write_message(
            &mut response_bytes,
            &Envelope::with_version(protocol::PROTOCOL_VERSION, Response::Pong),
        )
        .unwrap();
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!(
                "{}; dd bs=1 count={} of=/dev/null 2>/dev/null; {}",
                shell_printf(&handshake_bytes),
                request_bytes.len(),
                shell_printf(&response_bytes),
            ),
        ]);

        assert_eq!(
            run_helper_probe_command(command, Duration::from_secs(1)).unwrap(),
            handshake
        );
    }

    #[test]
    fn helper_probe_rejects_invalid_handshakes_and_timeouts() {
        let mut invalid = Command::new("sh");
        invalid.args(["-c", "printf 'NOTMAGIC'"]);
        assert_eq!(
            run_helper_probe_command(invalid, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let started = Instant::now();
        let mut timeout = Command::new("sh");
        timeout.args(["-c", "sleep 5"]);
        assert_eq!(
            run_helper_probe_command(timeout, Duration::from_millis(50))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn live_handshake_pins_identity_but_allows_compatible_version_changes() {
        let expected = FederationHandshake {
            version: FEDERATION_VERSION,
            node_id: Uuid::new_v4().to_string(),
            helper_version: "0.17.0".into(),
            core_protocol_version: protocol::MIN_PROTOCOL_VERSION,
            connection_mode: FederationConnectionMode::AdHoc,
        };
        let mut changed = expected.clone();
        changed.helper_version = "0.18.0".into();
        changed.core_protocol_version = protocol::PROTOCOL_VERSION;
        validate_live_handshake(&expected, &changed).unwrap();

        changed.node_id = Uuid::new_v4().to_string();
        assert_eq!(
            validate_live_handshake(&expected, &changed)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn helper_selection_skips_incompatible_discovered_executables() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let ssh = runtime.join("ssh");
        let handshake = FederationHandshake {
            version: FEDERATION_VERSION,
            node_id: Uuid::new_v4().to_string(),
            helper_version: "0.18.0".into(),
            core_protocol_version: protocol::PROTOCOL_VERSION,
            connection_mode: FederationConnectionMode::AdHoc,
        };
        let mut handshake_bytes = Vec::new();
        crate::federation::write_handshake(&mut handshake_bytes, &handshake).unwrap();
        let mut request_bytes = Vec::new();
        protocol::write_message(
            &mut request_bytes,
            &Envelope::with_version(protocol::PROTOCOL_VERSION, Request::Ping),
        )
        .unwrap();
        let mut response_bytes = Vec::new();
        protocol::write_message(
            &mut response_bytes,
            &Envelope::with_version(protocol::PROTOCOL_VERSION, Response::Pong),
        )
        .unwrap();
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in *'exec '*) last=${{last##*exec }} ;; esac\ncase \"$last\" in\n  \"'/bad/boomux' __federation-stdio\") printf 'NOTMAGIC' ;;\n  \"'/good/boomux' __federation-stdio\") {}; dd bs=1 count={} of=/dev/null 2>/dev/null; {} ;;\n  *) exit 64 ;;\nesac\n",
                shell_printf(&handshake_bytes),
                request_bytes.len(),
                shell_printf(&response_bytes),
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let candidates = [
            RemoteExecutable::parse("/bad/boomux").unwrap(),
            RemoteExecutable::parse("/good/boomux").unwrap(),
        ];

        let compatible = find_compatible_remote_helper_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            &candidates,
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        assert_eq!(compatible.executable, candidates[1]);
        assert_eq!(compatible.handshake, handshake);
        assert_eq!(
            fs::read_dir(&runtime)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("ssh-"))
                .count(),
            0
        );
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn bootstrap_plans_install_when_no_remote_executable_exists() {
        let runtime = runtime_directory();
        let ssh = write_bootstrap_ssh(&runtime, "", "");
        let plan = plan_remote_bootstrap_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let RemoteBootstrapPlan::Install(plan) = plan else {
            panic!("expected install plan");
        };
        assert_eq!(plan.reason, RemoteInstallReason::Missing);
        assert_eq!(plan.destination.as_str(), "/home/person/.local/bin/boomux");
        assert!(matches!(
            plan.source,
            RemoteInstallSource::CurrentBinary { .. }
        ));
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn bootstrap_plans_upgrade_for_a_known_old_protocol_helper() {
        let runtime = runtime_directory();
        let ssh = write_bootstrap_ssh(
            &runtime,
            "  \"'/old/boomux' __federation-stdio\") exit 2 ;;\n  \"'/old/boomux' --version\") printf 'boomux 0.14.2\\n' ;;",
            "/old/boomux\\0",
        );
        let plan = plan_remote_bootstrap_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        assert!(matches!(
            plan,
            RemoteBootstrapPlan::Install(RemoteInstallPlan {
                reason: RemoteInstallReason::Upgrade,
                ..
            })
        ));
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn bootstrap_keeps_a_compatible_candidate_when_an_old_one_is_also_discovered() {
        let runtime = runtime_directory();
        let node_id = Uuid::new_v4().to_string();
        let good = compatible_helper_script(&node_id);
        let ssh = write_bootstrap_ssh(
            &runtime,
            &format!(
                "  \"'/old/boomux' __federation-stdio\") exit 2 ;;\n  \"'/old/boomux' --version\") printf 'boomux 0.14.2\\n' ;;\n  \"'/good/boomux' __federation-stdio\") {good} ;;"
            ),
            "/old/boomux\\0/good/boomux\\0",
        );
        let plan = plan_remote_bootstrap_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let RemoteBootstrapPlan::Ready(helper) = plan else {
            panic!("expected compatible helper");
        };
        assert_eq!(helper.executable.as_str(), "/good/boomux");
        assert_eq!(helper.handshake.node_id, node_id);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn session_plan_keeps_a_compatible_helper_ready() {
        let runtime = runtime_directory();
        let node_id = Uuid::new_v4().to_string();
        let helper = compatible_helper_script(&node_id);
        let ssh = write_session_bootstrap_ssh(
            &runtime,
            &format!("  \"'/good/boomux' __federation-stdio\") {helper} ;;"),
            "/good/boomux\\0",
        );
        let mut session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();

        let RemoteBootstrapPlan::Ready(ready) = session.plan(Duration::from_secs(1)).unwrap()
        else {
            panic!("ordinary compatible planning must remain ready");
        };
        assert_eq!(ready.executable.as_str(), "/good/boomux");
        assert_eq!(ready.handshake.node_id, node_id);
        drop(session);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn registered_reauthentication_connects_only_the_pinned_existing_helper() {
        let runtime = runtime_directory();
        let node_id = Uuid::new_v4().to_string();
        let helper = compatible_helper_script(&node_id);
        let mutated = runtime.join("mutated");
        let ssh = write_session_bootstrap_ssh(
            &runtime,
            &format!(
                "  \"'/good/boomux' __federation-stdio\") {helper} ;;\n  *'boomux-install-transaction-v1'*|*'__bootstrap-activate'*|*'daemon restart'*) : > {}; exit 99 ;;",
                quote_posix_shell(mutated.to_str().unwrap())
            ),
            "/good/boomux\\0",
        );
        let session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let connection = session
            .connect_existing_verified(&node_id, Duration::from_secs(1))
            .unwrap();
        assert_eq!(connection.handshake.node_id, node_id);
        drop(connection);
        assert!(!mutated.exists());
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn registered_reauthentication_rejects_missing_and_changed_helpers_without_mutation() {
        let runtime = runtime_directory();
        let ssh = write_session_bootstrap_ssh(&runtime, "", "");
        let session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let error = match session
            .connect_existing_verified(&Uuid::new_v4().to_string(), Duration::from_secs(1))
        {
            Ok(_) => panic!("missing helper unexpectedly authenticated"),
            Err(error) => error,
        };
        assert_eq!(error_code(&error), "install_required");
        fs::remove_dir_all(&runtime).unwrap();

        let runtime = runtime_directory();
        let actual_node_id = Uuid::new_v4().to_string();
        let helper = compatible_helper_script(&actual_node_id);
        let mutated = runtime.join("mutated");
        let ssh = write_session_bootstrap_ssh(
            &runtime,
            &format!(
                "  \"'/good/boomux' __federation-stdio\") {helper} ;;\n  *'boomux-install-transaction-v1'*|*'__bootstrap-activate'*|*'daemon restart'*) : > {}; exit 99 ;;",
                quote_posix_shell(mutated.to_str().unwrap())
            ),
            "/good/boomux\\0",
        );
        let session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let error = match session
            .connect_existing_verified(&Uuid::new_v4().to_string(), Duration::from_secs(1))
        {
            Ok(_) => panic!("changed Node identity unexpectedly authenticated"),
            Err(error) => error,
        };
        assert_eq!(error_code(&error), "node_identity_changed");
        assert!(!mutated.exists());
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn explicit_upgrade_plans_replacement_for_a_compatible_registered_node() {
        let runtime = runtime_directory();
        let node_id = Uuid::new_v4().to_string();
        let helper = compatible_helper_script(&node_id);
        let ssh = write_session_bootstrap_ssh(
            &runtime,
            &format!("  \"'/good/boomux' __federation-stdio\") {helper} ;;"),
            "/good/boomux\\0",
        );
        let mut session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();

        let plan = session
            .plan_explicit_upgrade(&node_id, Duration::from_secs(1))
            .unwrap();
        assert_eq!(plan.reason, RemoteInstallReason::Upgrade);
        assert_eq!(
            plan.upgrade_helper.as_ref().unwrap().as_str(),
            "/good/boomux"
        );
        assert!(matches!(
            plan.source,
            RemoteInstallSource::CurrentBinary { .. }
        ));
        assert_eq!(
            plan.intent,
            RemoteInstallIntent::ExplicitRegisteredUpgrade {
                expected_node_id: node_id
            }
        );
        drop(session);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn active_bootstrap_recovery_is_busy_unless_an_alternate_helper_is_ready() {
        let runtime = runtime_directory();
        let ssh = write_session_bootstrap_ssh(&runtime, "", "");
        let script = fs::read_to_string(&ssh)
            .unwrap()
            .replace("boomux\\0clear\\0", "boomux\\0recovering\\0");
        fs::write(&ssh, script).unwrap();
        let mut session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let error = match session.plan(Duration::from_secs(1)) {
            Ok(_) => panic!("active recovery without another helper must be busy"),
            Err(error) => error,
        };
        assert_eq!(error_code(&error), "busy");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(
            recovery_disposition(&error),
            BootstrapRecoveryDisposition::NoRemoteMutation
        );
        drop(session);
        fs::remove_dir_all(&runtime).unwrap();

        let runtime = runtime_directory();
        let ssh = write_session_bootstrap_ssh(&runtime, "", "");
        let script = fs::read_to_string(&ssh)
            .unwrap()
            .replace("boomux\\0clear\\0", "boomux\\0stale\\0");
        fs::write(&ssh, script).unwrap();
        let mut session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let error = match session.plan(Duration::from_secs(1)) {
            Ok(_) => panic!("stale recovery must not propose another install"),
            Err(error) => error,
        };
        assert_eq!(error_code(&error), "upgrade_recovery_required");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(session);
        fs::remove_dir_all(&runtime).unwrap();

        let runtime = runtime_directory();
        let node_id = Uuid::new_v4().to_string();
        let helper = compatible_helper_script(&node_id);
        let ssh = write_session_bootstrap_ssh(
            &runtime,
            &format!("  \"'/good/boomux' __federation-stdio\") {helper} ;;"),
            "/good/boomux\\0",
        );
        let script = fs::read_to_string(&ssh)
            .unwrap()
            .replace("boomux\\0clear\\0", "boomux\\0recovering\\0");
        fs::write(&ssh, script).unwrap();
        let mut session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        assert!(matches!(
            session.plan(Duration::from_secs(1)).unwrap(),
            RemoteBootstrapPlan::Ready(_)
        ));
        drop(session);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn explicit_upgrade_rejects_a_different_node_before_remote_mutation() {
        let runtime = runtime_directory();
        let actual_node_id = Uuid::new_v4().to_string();
        let expected_node_id = Uuid::new_v4().to_string();
        let mutated = runtime.join("mutated");
        let helper = compatible_helper_script(&actual_node_id);
        let ssh = write_session_bootstrap_ssh(
            &runtime,
            &format!(
                "  \"'/good/boomux' __federation-stdio\") {helper} ;;\n  *'boomux-install-transaction-v1'*) : > {}; exit 99 ;;",
                quote_posix_shell(mutated.to_str().unwrap())
            ),
            "/good/boomux\\0",
        );
        let mut session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();

        let error = session
            .plan_explicit_upgrade(&expected_node_id, Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(error_code(&error), "node_identity_changed");
        assert!(!mutated.exists());
        drop(session);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn mixed_compatible_identities_fail_as_a_typed_conflict() {
        let runtime = runtime_directory();
        let first = compatible_helper_script(&Uuid::new_v4().to_string());
        let second = compatible_helper_script(&Uuid::new_v4().to_string());
        let ssh = write_bootstrap_ssh(
            &runtime,
            &format!(
                "  \"'/first/boomux' __federation-stdio\") {first} ;;\n  \"'/second/boomux' __federation-stdio\") {second} ;;"
            ),
            "/first/boomux\\0/second/boomux\\0",
        );
        let error = plan_remote_bootstrap_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .err()
        .expect("identity conflict must fail");
        assert_eq!(error_code(&error), "node_identity_conflict");
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn newer_helper_is_unsupported_and_never_an_upgrade_candidate() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let handshake = FederationHandshake {
            version: FEDERATION_VERSION,
            node_id: Uuid::new_v4().to_string(),
            helper_version: "future".into(),
            core_protocol_version: protocol::PROTOCOL_VERSION + 1,
            connection_mode: FederationConnectionMode::AdHoc,
        };
        let mut bytes = Vec::new();
        crate::federation::write_handshake(&mut bytes, &handshake).unwrap();
        let ssh = write_bootstrap_ssh(
            &runtime,
            &format!(
                "  \"'/future/boomux' __federation-stdio\") {} ;;\n  \"'/future/boomux' --version\") printf 'boomux 99.0.0\\n' ;;",
                shell_printf(&bytes)
            ),
            "/future/boomux\\0",
        );
        let error = plan_remote_bootstrap_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .err()
        .expect("newer helper must fail");
        assert_eq!(error_code(&error), "unsupported_version");
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn bootstrap_error_codes_preserve_failure_semantics() {
        for (error, code) in [
            (
                io::Error::new(io::ErrorKind::PermissionDenied, "SSH authentication failed"),
                "bootstrap_authentication_failed",
            ),
            (
                io::Error::new(io::ErrorKind::ConnectionAborted, "route EOF"),
                "bootstrap_transport_failed",
            ),
            (
                invalid_probe("remote helper returned an invalid header"),
                "bootstrap_malformed_helper",
            ),
            (
                RemotePlatform::parse_probe(b"boomux-platform-v1\0Linux\0mips\0").unwrap_err(),
                "bootstrap_unsupported_platform",
            ),
            (
                io::Error::other("remote Boomux install failed"),
                "bootstrap_install_failed",
            ),
        ] {
            assert_eq!(error_code(&error), code);
        }
    }

    #[test]
    fn stalled_candidate_cannot_starve_a_later_compatible_helper() {
        let runtime = runtime_directory();
        let node_id = Uuid::new_v4().to_string();
        let good = compatible_helper_script(&node_id);
        let ssh = write_bootstrap_ssh(
            &runtime,
            &format!(
                "  \"'/stalled/boomux' __federation-stdio\") sleep 2 ;;\n  \"'/good/boomux' __federation-stdio\") {good} ;;"
            ),
            "/stalled/boomux\\0/good/boomux\\0",
        );
        let plan = plan_remote_bootstrap_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_millis(400),
            ssh.as_os_str(),
        )
        .unwrap();
        let RemoteBootstrapPlan::Ready(helper) = plan else {
            panic!("expected compatible helper");
        };
        assert_eq!(helper.executable.as_str(), "/good/boomux");
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn inaccessible_and_malformed_candidates_do_not_authorize_replacement() {
        for (executable_case, expected_code) in [
            (
                "  \"'/bad/boomux' __federation-stdio\" | \"'/bad/boomux' --version\") exit 126 ;;",
                "bootstrap_transport_failed",
            ),
            (
                "  \"'/bad/boomux' __federation-stdio\") printf 'NOTMAGIC' ;;\n  \"'/bad/boomux' --version\") printf 'boomux 0.14.2\\n' ;;",
                "bootstrap_malformed_helper",
            ),
        ] {
            let runtime = runtime_directory();
            let ssh = write_bootstrap_ssh(&runtime, executable_case, "/bad/boomux\\0");
            let error = plan_remote_bootstrap_at(
                &runtime,
                None,
                SshTarget::parse("workbox").unwrap(),
                SshAuthenticationMode::Batch,
                Duration::from_secs(1),
                ssh.as_os_str(),
            )
            .err()
            .expect("candidate must fail closed");
            assert_eq!(error_code(&error), expected_code);
            fs::remove_dir_all(runtime).unwrap();
        }
    }

    #[test]
    fn current_published_release_is_selected_without_falling_back_to_stale_assets() {
        let release = select_published_release("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(release.tag, "v0.30.3");
        assert_eq!(release.protocol_version, 44);
        assert!(release.protocol_version <= protocol::PROTOCOL_VERSION);

        let error = select_published_release("aarch64-unknown-linux-gnu").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(
            error
                .to_string()
                .contains(&format!("local protocol {}", protocol::PROTOCOL_VERSION))
        );
        assert!(error.to_string().contains("manually stream"));
    }

    #[test]
    fn session_rolls_back_prior_helper_after_installed_binary_cannot_execute() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let remote = runtime.join("remote");
        fs::create_dir_all(&remote).unwrap();
        let destination = remote.join("boomux");
        let backup = remote.join("backup");
        let restart = remote.join("restart");
        fs::write(&destination, b"previous-helper").unwrap();
        let ssh = runtime.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\n{control}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *'boomux-install-transaction-v1'*) mv {destination} {backup}; cat > {destination}; printf 'boomux-install-transaction-v1\\0.boomux.bootstrap.ABC12345\\0' ;;\n  *'transaction/watchdog_pid'*'restore_install'*) cat >/dev/null; rm -f {destination}; mv {backup} {destination} ;;\n  *'daemon status --json'*) exit 126 ;;\n  *'daemon restart'*) : > {restart} ;;\n  *\"'/home/person/.local/bin/boomux' __federation-stdio\" | \"'/home/person/.local/bin/boomux' --version\") exit 126 ;;\n  *) exit 64 ;;\nesac\n",
                control = CONTROL_MASTER_SCRIPT,
                destination = quote_posix_shell(destination.to_str().unwrap()),
                backup = quote_posix_shell(backup.to_str().unwrap()),
                restart = quote_posix_shell(restart.to_str().unwrap()),
            ),
        )
        .unwrap();
        let script = fs::read_to_string(&ssh)
            .unwrap()
            .replace(
                &format!(
                    "mv {} {}; cat > {};",
                    quote_posix_shell(destination.to_str().unwrap()),
                    quote_posix_shell(backup.to_str().unwrap()),
                    quote_posix_shell(destination.to_str().unwrap())
                ),
                "cat >/dev/null;",
            )
            .replace(
                &format!(
                    "cat >/dev/null; rm -f {}; mv {} {};",
                    quote_posix_shell(destination.to_str().unwrap()),
                    quote_posix_shell(backup.to_str().unwrap()),
                    quote_posix_shell(destination.to_str().unwrap())
                ),
                "cat >/dev/null;",
            )
            .replace(
                &format!(
                    "rm -f {}; mv {} {}",
                    quote_posix_shell(destination.to_str().unwrap()),
                    quote_posix_shell(backup.to_str().unwrap()),
                    quote_posix_shell(destination.to_str().unwrap())
                ),
                ":",
            );
        fs::write(&ssh, script).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let plan = RemoteInstallPlan {
            target: SshTarget::parse("workbox").unwrap(),
            destination: RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            source: RemoteInstallSource::CurrentBinary {
                path: runtime.join("pinned"),
                sha256: format!("{:x}", Sha256::digest(b"wrong-abi")),
                bytes: b"wrong-abi".to_vec(),
            },
            reason: RemoteInstallReason::Upgrade,
            bootstrap_id: Some(session.id),
            upgrade_helper: Some(
                RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            ),
            intent: RemoteInstallIntent::AutomaticCompatibility,
        };

        assert!(
            session
                .install_and_connect(&plan, Duration::from_secs(1))
                .is_err()
        );
        assert_eq!(fs::read(&destination).unwrap(), b"previous-helper");
        assert!(!backup.exists());
        assert!(!restart.exists());
        assert_eq!(
            fs::read_dir(&runtime)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("ssh-"))
                .count(),
            0
        );
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn post_install_protocol_ping_eof_rolls_back_before_commit() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let destination = runtime.join("destination");
        let backup = runtime.join("backup");
        let count = runtime.join("count");
        let committed = runtime.join("committed");
        fs::write(&destination, b"previous-helper").unwrap();
        let handshake = FederationHandshake {
            version: FEDERATION_VERSION,
            node_id: Uuid::new_v4().to_string(),
            helper_version: "test".into(),
            core_protocol_version: protocol::PROTOCOL_VERSION,
            connection_mode: FederationConnectionMode::AdHoc,
        };
        let mut handshake_bytes = Vec::new();
        crate::federation::write_handshake(&mut handshake_bytes, &handshake).unwrap();
        let mut request = Vec::new();
        protocol::write_message(
            &mut request,
            &Envelope::with_version(protocol::PROTOCOL_VERSION, Request::Ping),
        )
        .unwrap();
        let mut pong = Vec::new();
        protocol::write_message(
            &mut pong,
            &Envelope::with_version(protocol::PROTOCOL_VERSION, Response::Pong),
        )
        .unwrap();
        let ssh = runtime.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\n{control}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *'boomux-install-transaction-v1'*) mv {destination} {backup}; cat > {destination}; printf 'boomux-install-transaction-v1\\0.boomux.bootstrap.ABC12345\\0' ;;\n  *'transaction/watchdog_pid'*'restore_install'*) cat >/dev/null; rm -f {destination}; mv {backup} {destination} ;;\n  *'daemon status --json'*) printf '%s' '{{\"schema\":\"boomux.cli/v1\",\"command\":\"daemon.status\",\"data\":{{\"protocol_version\":38}}}}' ;;\n  *'daemon restart'*) exit 99 ;;\n  *'rm -f \"$transaction/backup\"'*) cat >/dev/null; : > {committed} ;;\n  *\"'/home/person/.local/bin/boomux' __federation-stdio\") n=0; [ ! -f {count} ] || n=$(cat {count}); n=$((n + 1)); printf '%s' \"$n\" > {count}; {handshake}; dd bs=1 count={request_len} of=/dev/null 2>/dev/null; [ \"$n\" -eq 1 ] && {pong} ;;\n  *) exit 64 ;;\nesac\n",
                control = CONTROL_MASTER_SCRIPT,
                destination = quote_posix_shell(destination.to_str().unwrap()),
                backup = quote_posix_shell(backup.to_str().unwrap()),
                committed = quote_posix_shell(committed.to_str().unwrap()),
                count = quote_posix_shell(count.to_str().unwrap()),
                handshake = shell_printf(&handshake_bytes),
                request_len = request.len(),
                pong = shell_printf(&pong),
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        add_fake_daemon_identity(&ssh, "/home/person/.local/bin/boomux");
        let session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let plan = RemoteInstallPlan {
            target: SshTarget::parse("workbox").unwrap(),
            destination: RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            source: RemoteInstallSource::CurrentBinary {
                path: runtime.join("pinned"),
                sha256: format!("{:x}", Sha256::digest(b"replacement")),
                bytes: b"replacement".to_vec(),
            },
            reason: RemoteInstallReason::Upgrade,
            bootstrap_id: Some(session.id),
            upgrade_helper: Some(
                RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            ),
            intent: RemoteInstallIntent::AutomaticCompatibility,
        };
        let error = session
            .install_and_connect(&plan, Duration::from_secs(1))
            .err()
            .expect("post-install ping EOF must fail");
        assert_eq!(error_code(&error), "bootstrap_transport_failed");
        assert_eq!(
            recovery_disposition(&error),
            BootstrapRecoveryDisposition::RollbackConfirmed
        );
        assert_eq!(fs::read(&destination).unwrap(), b"previous-helper");
        assert!(!backup.exists());
        assert!(!committed.exists());
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn compatible_running_daemon_is_checked_without_restart_before_commit() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let destination = runtime.join("destination");
        let backup = runtime.join("backup");
        let count = runtime.join("count");
        let committed = runtime.join("committed");
        let status_checked = runtime.join("status-checked");
        fs::write(&destination, b"previous-helper").unwrap();
        let helper = compatible_helper_script(&Uuid::new_v4().to_string());
        let ssh = runtime.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\n{control}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *'boomux-install-transaction-v1'*) mv {destination} {backup}; cat > {destination}; printf 'boomux-install-transaction-v1\\0.boomux.bootstrap.ABC12345\\0' ;;\n  *'committed=$lock/committed'*) cat >/dev/null; [ \"$(cat {count})\" -eq 2 ]; : > {committed}; printf 'boomux-install-commit-v1\\0committed\\0' ;;\n  *'transaction/watchdog_pid'*'daemon stop'*) exit 70 ;;\n  *'daemon status --json'*) : > {status_checked}; printf '%s' '{{\"schema\":\"boomux.cli/v1\",\"command\":\"daemon.status\",\"data\":{{\"protocol_version\":38}}}}' ;;\n  *'daemon restart'*) exit 99 ;;\n  *\"'/home/person/.local/bin/boomux' __federation-stdio\") n=0; [ ! -f {count} ] || n=$(cat {count}); n=$((n + 1)); printf '%s' \"$n\" > {count}; {helper} ;;\n  *) exit 64 ;;\nesac\n",
                control = CONTROL_MASTER_SCRIPT,
                destination = quote_posix_shell(destination.to_str().unwrap()),
                backup = quote_posix_shell(backup.to_str().unwrap()),
                count = quote_posix_shell(count.to_str().unwrap()),
                committed = quote_posix_shell(committed.to_str().unwrap()),
                status_checked = quote_posix_shell(status_checked.to_str().unwrap()),
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        add_fake_daemon_identity(&ssh, "/home/person/.local/bin/boomux");
        let session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let plan = RemoteInstallPlan {
            target: SshTarget::parse("workbox").unwrap(),
            destination: RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            source: RemoteInstallSource::CurrentBinary {
                path: runtime.join("pinned"),
                sha256: format!("{:x}", Sha256::digest(b"replacement")),
                bytes: b"replacement".to_vec(),
            },
            reason: RemoteInstallReason::Upgrade,
            bootstrap_id: Some(session.id),
            upgrade_helper: Some(
                RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            ),
            intent: RemoteInstallIntent::AutomaticCompatibility,
        };
        let connection = session
            .install_and_connect(&plan, Duration::from_secs(1))
            .unwrap();
        assert!(committed.exists());
        assert!(status_checked.exists());
        assert!(backup.exists());
        assert_eq!(fs::read_to_string(&count).unwrap(), "2");
        drop(connection);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn explicit_upgrade_gracefully_restarts_a_compatible_running_daemon() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let destination = runtime.join("destination");
        let backup = runtime.join("backup");
        let count = runtime.join("count");
        let committed = runtime.join("committed");
        let restarted = runtime.join("restarted");
        fs::write(&destination, b"previous-helper").unwrap();
        let node_id = Uuid::new_v4().to_string();
        let helper = compatible_helper_script(&node_id);
        let ssh = runtime.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\n{control}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *'boomux-install-transaction-v1'*) mv {destination} {backup}; cat > {destination}; printf 'boomux-install-transaction-v1\\0.boomux.bootstrap.ABC12345\\0' ;;\n  *'committed=$lock/committed'*) cat >/dev/null; [ \"$(cat {count})\" -eq 3 ]; : > {committed}; printf 'boomux-install-commit-v1\\0committed\\0' ;;\n  *': > \"$transaction/restarted\"'*) cat >/dev/null; : > {restarted} ;;\n  *'transaction/watchdog_pid'*'restore_install'*) cat >/dev/null; rm -f {destination}; mv {backup} {destination} ;;\n  *'daemon status --json'*) printf '%s' '{{\"schema\":\"boomux.cli/v1\",\"command\":\"daemon.status\",\"data\":{{\"protocol_version\":38}}}}' ;;\n  *'daemon restart'*) : > {restarted} ;;\n  *\"'/home/person/.local/bin/boomux' __federation-stdio\") n=0; [ ! -f {count} ] || n=$(cat {count}); n=$((n + 1)); printf '%s' \"$n\" > {count}; {helper} ;;\n  *) exit 64 ;;\nesac\n",
                control = CONTROL_MASTER_SCRIPT,
                destination = quote_posix_shell(destination.to_str().unwrap()),
                backup = quote_posix_shell(backup.to_str().unwrap()),
                count = quote_posix_shell(count.to_str().unwrap()),
                committed = quote_posix_shell(committed.to_str().unwrap()),
                restarted = quote_posix_shell(restarted.to_str().unwrap()),
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        add_fake_daemon_identity(&ssh, "/home/person/.local/bin/boomux");
        let session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let plan = RemoteInstallPlan {
            target: SshTarget::parse("workbox").unwrap(),
            destination: RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            source: RemoteInstallSource::CurrentBinary {
                path: runtime.join("pinned"),
                sha256: format!("{:x}", Sha256::digest(b"replacement")),
                bytes: b"replacement".to_vec(),
            },
            reason: RemoteInstallReason::Upgrade,
            bootstrap_id: Some(session.id),
            upgrade_helper: Some(
                RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            ),
            intent: RemoteInstallIntent::ExplicitRegisteredUpgrade {
                expected_node_id: node_id,
            },
        };

        let connection = session
            .install_and_connect(&plan, Duration::from_secs(1))
            .unwrap();
        assert!(restarted.exists());
        assert!(committed.exists());
        assert_eq!(fs::read_to_string(&count).unwrap(), "3");
        drop(connection);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn explicit_upgrade_rolls_back_post_activation_node_identity_mismatch() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let destination = runtime.join("destination");
        let backup = runtime.join("backup");
        let committed = runtime.join("committed");
        let restarted = runtime.join("restarted");
        fs::write(&destination, b"previous-helper").unwrap();
        let expected_node_id = Uuid::new_v4().to_string();
        let changed_helper = compatible_helper_script(&Uuid::new_v4().to_string());
        let ssh = runtime.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\n{control}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *'boomux-install-transaction-v1'*) mv {destination} {backup}; cat > {destination}; printf 'boomux-install-transaction-v1\\0.boomux.bootstrap.ABC12345\\0' ;;\n  *'transaction/watchdog_pid'*'restore_install'*) cat >/dev/null; rm -f {destination}; mv {backup} {destination} ;;\n  *'committed=$lock/committed'*) cat >/dev/null; : > {committed}; printf 'boomux-install-commit-v1\\0committed\\0' ;;\n  *'daemon status --json'*) printf '%s' '{{\"schema\":\"boomux.cli/v1\",\"command\":\"daemon.status\",\"data\":{{\"protocol_version\":38}}}}' ;;\n  *'daemon restart'*) : > {restarted} ;;\n  *\"'/home/person/.local/bin/boomux' __federation-stdio\") {changed_helper} ;;\n  *) exit 64 ;;\nesac\n",
                control = CONTROL_MASTER_SCRIPT,
                destination = quote_posix_shell(destination.to_str().unwrap()),
                backup = quote_posix_shell(backup.to_str().unwrap()),
                committed = quote_posix_shell(committed.to_str().unwrap()),
                restarted = quote_posix_shell(restarted.to_str().unwrap()),
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        add_fake_daemon_identity(&ssh, "/home/person/.local/bin/boomux");
        let session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let plan = RemoteInstallPlan {
            target: SshTarget::parse("workbox").unwrap(),
            destination: RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            source: RemoteInstallSource::CurrentBinary {
                path: runtime.join("pinned"),
                sha256: format!("{:x}", Sha256::digest(b"replacement")),
                bytes: b"replacement".to_vec(),
            },
            reason: RemoteInstallReason::Upgrade,
            bootstrap_id: Some(session.id),
            upgrade_helper: Some(
                RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            ),
            intent: RemoteInstallIntent::ExplicitRegisteredUpgrade { expected_node_id },
        };

        let error = session
            .install_and_connect(&plan, Duration::from_secs(1))
            .err()
            .expect("changed installed Node identity must fail");
        assert_eq!(error_code(&error), "node_identity_changed");
        assert_eq!(fs::read(&destination).unwrap(), b"previous-helper");
        assert!(!backup.exists());
        assert!(!committed.exists());
        assert!(!restarted.exists());
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn upgrade_with_absent_daemon_refuses_before_activation() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let destination = runtime.join("destination");
        let backup = runtime.join("backup");
        let count = runtime.join("count");
        let started = runtime.join("started-by-helper");
        let restarted = runtime.join("explicit-restart");
        let committed = runtime.join("committed");
        fs::write(&destination, b"previous-helper").unwrap();
        let helper = compatible_helper_script(&Uuid::new_v4().to_string());
        let ssh = runtime.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\n{control}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *'boomux-install-transaction-v1'*) mv {destination} {backup}; cat > {destination}; printf 'boomux-install-transaction-v1\\0.boomux.bootstrap.ABC12345\\0' ;;\n  *'transaction/watchdog_pid'*'restore_install'*) cat >/dev/null; rm -f {destination}; mv {backup} {destination} ;;\n  *': > \"$transaction/daemon_absent\"'*) cat >/dev/null ;;\n  *'committed=$lock/committed'*) cat >/dev/null; [ \"$(cat {count})\" -eq 2 ]; : > {committed}; printf 'boomux-install-commit-v1\\0committed\\0' ;;\n  *'daemon status --json'*) if [ -e {started} ]; then printf '%s' '{{\"schema\":\"boomux.cli/v1\",\"command\":\"daemon.status\",\"data\":{{\"protocol_version\":38}}}}'; else printf 'boomux-daemon-status-v1\\0absent\\0'; fi ;;\n  *'daemon restart'*) : > {restarted} ;;\n  *\"'/home/person/.local/bin/boomux' __federation-stdio\") : > {started}; n=0; [ ! -f {count} ] || n=$(cat {count}); n=$((n + 1)); printf '%s' \"$n\" > {count}; {helper} ;;\n  *) exit 64 ;;\nesac\n",
                control = CONTROL_MASTER_SCRIPT,
                destination = quote_posix_shell(destination.to_str().unwrap()),
                backup = quote_posix_shell(backup.to_str().unwrap()),
                count = quote_posix_shell(count.to_str().unwrap()),
                started = quote_posix_shell(started.to_str().unwrap()),
                restarted = quote_posix_shell(restarted.to_str().unwrap()),
                committed = quote_posix_shell(committed.to_str().unwrap()),
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let plan = RemoteInstallPlan {
            target: SshTarget::parse("workbox").unwrap(),
            destination: RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            source: RemoteInstallSource::CurrentBinary {
                path: runtime.join("pinned"),
                sha256: format!("{:x}", Sha256::digest(b"replacement")),
                bytes: b"replacement".to_vec(),
            },
            reason: RemoteInstallReason::Upgrade,
            bootstrap_id: Some(session.id),
            upgrade_helper: Some(
                RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            ),
            intent: RemoteInstallIntent::AutomaticCompatibility,
        };
        let error = session
            .install_and_connect(&plan, Duration::from_secs(1))
            .err()
            .expect("upgrade requires a running daemon identity");
        assert_eq!(error_code(&error), "upgrade_required");
        assert_eq!(
            recovery_disposition(&error),
            BootstrapRecoveryDisposition::RollbackConfirmed
        );
        assert!(!restarted.exists());
        assert!(!committed.exists());
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn unset_runtime_old_daemon_upgrade_restarts_verifies_pings_and_commits() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let destination = runtime.join("destination");
        let backup = runtime.join("backup");
        let socket = runtime.join("remote-runtime/boomux/daemon.sock");
        let restarted = runtime.join("restarted");
        let committed = runtime.join("committed");
        let count = runtime.join("helper-count");
        let log = runtime.join("stages");
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        fs::write(&socket, b"existing-socket").unwrap();
        fs::write(&destination, b"protocol-21-helper").unwrap();

        let old_handshake = FederationHandshake {
            version: FEDERATION_VERSION,
            node_id: Uuid::new_v4().to_string(),
            helper_version: "old".into(),
            core_protocol_version: protocol::PROTOCOL_VERSION,
            connection_mode: FederationConnectionMode::AdHoc,
        };
        let current_helper = compatible_helper_script(&old_handshake.node_id);
        let ssh = runtime.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\n{control}\nunset XDG_RUNTIME_DIR\nlast=\nfor arg do last=$arg; done\nrequire_runtime() {{ case \"$last\" in *boomux-runtime-v1*'/run/user/$boomux_uid'*'export XDG_RUNTIME_DIR'*) [ -e {socket} ] ;; *) return 1 ;; esac; }}\ncase \"$last\" in\n  *'boomux-install-transaction-v1'*) mv {destination} {backup}; cat > {destination}; printf 'boomux-install-transaction-v1\\0.boomux.bootstrap.ABC12345\\0'; printf 'install\\n' >> {log} ;;\n  *': > \"$transaction/restarted\"'*) cat >/dev/null; : > {restarted}; printf 'mark-restarted\\n' >> {log} ;;\n  *'committed=$lock/committed'*) cat >/dev/null; [ \"$(cat {count})\" -eq 3 ]; : > {committed}; printf 'commit\\n' >> {log}; printf 'boomux-install-commit-v1\\0committed\\0' ;;\n  *'daemon status --json'*) require_runtime || exit 97; [ -e {socket} ] || exit 98; printf 'status\\n' >> {log}; printf '%s' '{{\"schema\":\"boomux.cli/v1\",\"command\":\"daemon.status\",\"data\":{{\"protocol_version\":21}}}}' ;;\n  *'daemon restart'*) require_runtime || exit 97; [ -e {socket} ] || exit 98; : > {restarted}; printf 'restart\\n' >> {log} ;;\n  *\"'/home/person/.local/bin/boomux' __federation-stdio\") require_runtime || exit 97; [ -e {socket} ] || exit 98; n=0; [ ! -e {count} ] || n=$(cat {count}); n=$((n + 1)); printf '%s' \"$n\" > {count}; if [ \"$n\" -eq 1 ]; then printf 'helper-old\\n' >> {log}; {old_handshake}; else printf 'helper-current\\n' >> {log}; {current_helper}; fi ;;\n  \"'/home/person/.local/bin/boomux' --version\") printf 'version\\n' >> {log}; printf 'boomux {version}\\n' ;;\n  *) exit 64 ;;\nesac\n",
                control = CONTROL_MASTER_SCRIPT,
                socket = quote_posix_shell(socket.to_str().unwrap()),
                destination = quote_posix_shell(destination.to_str().unwrap()),
                backup = quote_posix_shell(backup.to_str().unwrap()),
                restarted = quote_posix_shell(restarted.to_str().unwrap()),
                committed = quote_posix_shell(committed.to_str().unwrap()),
                count = quote_posix_shell(count.to_str().unwrap()),
                log = quote_posix_shell(log.to_str().unwrap()),
                old_handshake = current_helper,
                version = env!("CARGO_PKG_VERSION"),
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        add_fake_daemon_identity(&ssh, "/home/person/.local/bin/boomux");
        let session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let plan = RemoteInstallPlan {
            target: SshTarget::parse("workbox").unwrap(),
            destination: RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            source: RemoteInstallSource::CurrentBinary {
                path: runtime.join("pinned"),
                sha256: format!("{:x}", Sha256::digest(b"replacement")),
                bytes: b"replacement".to_vec(),
            },
            reason: RemoteInstallReason::Upgrade,
            bootstrap_id: Some(session.id),
            upgrade_helper: Some(
                RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            ),
            intent: RemoteInstallIntent::AutomaticCompatibility,
        };
        let connection = session
            .install_and_connect(&plan, Duration::from_secs(1))
            .unwrap();
        assert!(restarted.exists());
        assert!(committed.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"replacement");
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "install\nstatus\nhelper-old\nstatus\nmark-restarted\nrestart\nhelper-current\nhelper-current\ncommit\n"
        );
        drop(connection);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn lost_commit_ack_is_unknown_and_exact_retry_discovers_installed_helper() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let destination = runtime.join("destination");
        let backup = runtime.join("backup");
        let installed = runtime.join("installed");
        let committed = runtime.join("committed");
        fs::write(&destination, b"previous-helper").unwrap();
        let helper = compatible_helper_script(&Uuid::new_v4().to_string());
        let ssh = runtime.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\n{control}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0'; [ -e {installed} ] && printf '/home/person/.local/bin/boomux\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/person/.local/bin/boomux\\0' ;;\n  *'boomux-install-transaction-v1'*) mv {destination} {backup}; cat > {destination}; : > {installed}; printf 'boomux-install-transaction-v1\\0.boomux.bootstrap.ABC12345\\0' ;;\n  *'committed=$lock/committed'*) cat >/dev/null; : > {committed}; printf 'boomux-install-commit-v1\\0committed'; exit 255 ;;\n  *'daemon status --json'*) printf '%s' '{{\"schema\":\"boomux.cli/v1\",\"command\":\"daemon.status\",\"data\":{{\"protocol_version\":38}}}}' ;;\n  *'daemon restart'*) exit 99 ;;\n  *\"'/home/person/.local/bin/boomux' __federation-stdio\") {helper} ;;\n  *) exit 64 ;;\nesac\n",
                control = CONTROL_MASTER_SCRIPT,
                destination = quote_posix_shell(destination.to_str().unwrap()),
                backup = quote_posix_shell(backup.to_str().unwrap()),
                installed = quote_posix_shell(installed.to_str().unwrap()),
                committed = quote_posix_shell(committed.to_str().unwrap()),
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        add_fake_daemon_identity(&ssh, "/home/person/.local/bin/boomux");

        let session = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let plan = RemoteInstallPlan {
            target: SshTarget::parse("workbox").unwrap(),
            destination: RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            source: RemoteInstallSource::CurrentBinary {
                path: runtime.join("pinned"),
                sha256: format!("{:x}", Sha256::digest(b"replacement")),
                bytes: b"replacement".to_vec(),
            },
            reason: RemoteInstallReason::Upgrade,
            bootstrap_id: Some(session.id),
            upgrade_helper: Some(
                RemoteExecutable::parse("/home/person/.local/bin/boomux").unwrap(),
            ),
            intent: RemoteInstallIntent::AutomaticCompatibility,
        };
        let error = session
            .install_and_connect(&plan, Duration::from_secs(1))
            .err()
            .expect("lost commit acknowledgment must be unknown");
        assert_eq!(error_code(&error), "bootstrap_commit_outcome_unknown");
        assert_eq!(
            recovery_disposition(&error),
            BootstrapRecoveryDisposition::OutcomeUnknown
        );
        assert!(committed.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"replacement");
        assert!(backup.exists());

        let mut retry = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let RemoteBootstrapPlan::Ready(helper) = retry.plan(Duration::from_secs(1)).unwrap() else {
            panic!("exact retry must discover the committed compatible helper");
        };
        let mut connection = retry.connect(helper, Duration::from_secs(1)).unwrap();
        connection
            .ping_with_timeout(Duration::from_secs(1))
            .unwrap();
        drop(connection);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn observation_from_one_master_cannot_authorize_another_route() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let ssh = runtime.join("ssh");
        fs::write(
            &ssh,
            format!("#!/bin/sh\n{CONTROL_MASTER_SCRIPT}\nexit 64\n"),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let first = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let second = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let helper = CompatibleRemoteHelper {
            executable: RemoteExecutable::parse("/remote/boomux").unwrap(),
            handshake: FederationHandshake {
                version: FEDERATION_VERSION,
                node_id: Uuid::new_v4().to_string(),
                helper_version: "test".into(),
                core_protocol_version: protocol::PROTOCOL_VERSION,
                connection_mode: FederationConnectionMode::AdHoc,
            },
            bootstrap_id: Some(first.id),
        };
        let error = second
            .connect(helper, Duration::from_secs(1))
            .err()
            .expect("cross-master observation must fail");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("different bootstrap endpoint"));
        drop(first);
        assert_eq!(
            fs::read_dir(&runtime)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("ssh-"))
                .count(),
            0
        );
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn discovery_runs_fixed_probes_through_the_real_ssh_argv_boundary() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let log = runtime.join("arguments");
        let ssh = runtime.join("ssh");
        let quoted_log = quote_posix_shell(log.to_str().unwrap());
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nprintf 'call\\0' >> {quoted_log}\nfor arg do printf '%s\\0' \"$arg\" >> {quoted_log}; done\nprintf 'end\\0' >> {quoted_log}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0/usr/bin/boomux\\0/opt/homebrew/bin/boomux\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/person/.local/bin/boomux\\0' ;;\n  *) exit 64 ;;\nesac\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();

        let discovery = discover_remote_at(
            &runtime,
            Some(Path::new("/home/person/.ssh/config")),
            SshTarget::parse("user@workbox").unwrap(),
            SshAuthenticationMode::Interactive,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        assert_eq!(
            discovery.platform,
            RemotePlatform {
                operating_system: RemoteOperatingSystem::Linux,
                architecture: RemoteArchitecture::X86_64,
            }
        );
        assert_eq!(
            discovery
                .executables
                .iter()
                .map(RemoteExecutable::as_str)
                .collect::<Vec<_>>(),
            ["/usr/bin/boomux", "/opt/homebrew/bin/boomux"]
        );
        assert_eq!(
            discovery.install_destination.as_str(),
            "/home/person/.local/bin/boomux"
        );

        let arguments = fs::read(&log).unwrap();
        let fields = arguments
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .map(|field| std::str::from_utf8(field).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(fields.iter().filter(|field| **field == "call").count(), 3);
        assert_eq!(
            fields
                .iter()
                .filter(|field| **field == "user@workbox")
                .count(),
            3
        );
        assert!(fields.contains(&PLATFORM_PROBE_COMMAND));
        assert!(fields.contains(&EXECUTABLE_PROBE_COMMAND));
        assert!(fields.contains(&INSTALL_DESTINATION_PROBE_COMMAND));
        assert_eq!(
            fs::read_dir(&runtime)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("ssh-"))
                .count(),
            0
        );
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn fixed_install_command_streams_private_executable_and_replaces_atomically() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join(".local/bin")).unwrap();
        let destination = directory.join(".local/bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let install =
            gate_watchdog(&REMOTE_INSTALL_COMMAND.replace("lease_limit=180", "lease_limit=1"));
        let transaction = run_local_upload_with_command(&directory, b"replacement", "sh", &install);
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        run_local_activation(&directory, "sh", &transaction, RemoteInstallReason::Missing);
        assert_eq!(fs::read(&destination).unwrap(), b"replacement");
        assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o777, 0o755);
        let commit = REMOTE_INSTALL_COMMIT_COMMAND.replace("sleep 180", "sleep 3");
        let mut child = local_shell_command("sh", &directory);
        child.args(["-c", &commit]);
        let output =
            run_streaming_command_capture(child, transaction.input(), Duration::from_secs(1))
                .unwrap();
        parse_install_commit(&output.stdout).unwrap();
        let mut retry = local_shell_command("sh", &directory);
        retry.args(["-c", &commit]);
        let retry =
            run_streaming_command_capture(retry, transaction.input(), Duration::from_secs(1))
                .unwrap();
        parse_install_commit(&retry.stdout).unwrap();
        assert!(
            directory
                .join(".local/bin/.boomux.bootstrap.lock/committed/backup")
                .exists()
        );
        directory.reap_watchdogs();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn commit_transport_loss_at_every_step_preserves_atomic_outcome_and_cleans_lock() {
        let cuts = [
            ("directory=$HOME/.local/bin;", false),
            ("[ \"$(/bin/cat \"$lock/id\")\" = \"$txn\" ];", false),
            ("claim_acquire || exit 73;", false),
            ("/bin/mv \"$transaction\" \"$committed\";", true),
            ("trap - EXIT HUP INT TERM; claim_release;", true),
            (
                "claim_release; printf 'boomux-install-commit-v1\\0committed\\0'",
                true,
            ),
        ];
        for (needle, committed) in cuts {
            let directory = runtime_directory();
            fs::create_dir_all(directory.join(".local/bin")).unwrap();
            let destination = directory.join(".local/bin/boomux");
            fs::write(&destination, b"previous").unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();

            let install = REMOTE_INSTALL_COMMAND
                .replace("lease_limit=180", "lease_limit=1")
                .replace("[ \"$claim_age\" -ge 180 ]", "[ \"$claim_age\" -ge 1 ]");
            let install = gate_watchdog(&install);
            let delayed_install = install.replacen(
                "while :; do if claim_pid_override=",
                "while [ ! -e \"$HOME/watchdog-pid-release\" ]; do /bin/sleep 0.01; done; while :; do if claim_pid_override=",
                1,
            );
            assert_ne!(delayed_install, install);
            let transaction =
                run_local_upload_with_command(&directory, b"replacement", "sh", &delayed_install);
            run_local_activation(&directory, "sh", &transaction, RemoteInstallReason::Missing);

            let commit_base = REMOTE_INSTALL_COMMIT_COMMAND.replace("sleep 180", "sleep 1");
            let injected = format!("{}; kill -KILL $$;", needle.trim_end_matches(';'));
            let commit_command = commit_base.replacen(needle, &injected, 1);
            assert_ne!(commit_command, commit_base);
            let mut child = local_shell_command("sh", &directory);
            child.args(["-c", &commit_command]);
            assert!(
                run_streaming_command_capture(child, transaction.input(), Duration::from_secs(1),)
                    .is_err(),
                "commit unexpectedly acknowledged after cut at {needle}"
            );
            fs::write(directory.join("watchdog-pid-release"), b"").unwrap();
            fs::write(directory.join("watchdog-tick"), b"").unwrap();

            let lock = directory.join(".local/bin/.boomux.bootstrap.lock");
            let deadline = Instant::now() + Duration::from_secs(15);
            while lock.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(!lock.exists(), "stale lock after cut at {needle}");
            assert_eq!(
                fs::read(&destination).unwrap(),
                if committed {
                    &b"replacement"[..]
                } else {
                    &b"previous"[..]
                },
                "wrong outcome after cut at {needle}"
            );
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn post_install_failure_atomically_restores_previous_or_missing_destination() {
        for previous in [Some(&b"previous"[..]), None] {
            let directory = runtime_directory();
            fs::create_dir_all(directory.join(".local/bin")).unwrap();
            let destination = directory.join(".local/bin/boomux");
            if let Some(previous) = previous {
                fs::write(&destination, previous).unwrap();
                fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
            }
            let transaction = run_local_install(&directory, b"incompatible-abi");
            assert_eq!(fs::read(&destination).unwrap(), b"incompatible-abi");

            run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
            match previous {
                Some(previous) => assert_eq!(fs::read(&destination).unwrap(), previous),
                None => assert!(!destination.exists()),
            }
            assert!(
                fs::read_dir(directory.join(".local/bin"))
                    .unwrap()
                    .filter_map(Result::ok)
                    .all(|entry| !entry.file_name().to_string_lossy().starts_with(".boomux."))
            );
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn concurrent_install_transaction_fails_busy_without_touching_the_first() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join(".local/bin")).unwrap();
        let destination = directory.join(".local/bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let first = run_local_install(&directory, b"first-provisional");

        let mut second = Command::new("sh");
        second
            .args(["-c", REMOTE_INSTALL_COMMAND])
            .env("HOME", &directory);
        let second_transaction = InstallTransactionId::generate();
        let error = run_streaming_command_capture(
            second,
            second_transaction.upload_input(b"second-provisional"),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error_code(&error), "busy");
        assert_eq!(fs::read(&destination).unwrap(), b"first-provisional");
        assert!(directory.join(".local/bin/.boomux.bootstrap.lock").is_dir());

        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &first);
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dead_atomic_claim_is_reclaimed_and_release_is_owner_checked() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join(".local/bin")).unwrap();
        let destination = directory.join(".local/bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let install = gate_watchdog(
            &REMOTE_INSTALL_COMMAND
                .replace("lease_limit=180", "lease_limit=1")
                .replace("[ \"$claim_age\" -ge 180 ]", "[ \"$claim_age\" -ge 1 ]"),
        );
        let _transaction =
            run_local_upload_with_command(&directory, b"replacement", "sh", &install);
        let claim = directory.join(".local/bin/.boomux.bootstrap.lock/claim");
        fs::write(&claim, b"dead-owner\n999999999\n1\n0\n").unwrap();
        fs::write(directory.join("watchdog-tick"), b"").unwrap();
        let lock = directory.join(".local/bin/.boomux.bootstrap.lock");
        let deadline = Instant::now() + Duration::from_secs(3);
        while lock.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!lock.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"previous");

        fs::remove_file(directory.join("watchdog-tick")).unwrap();
        let install = gate_watchdog(REMOTE_INSTALL_COMMAND);
        let transaction = run_local_upload_with_command(&directory, b"replacement", "sh", &install);
        let renew = REMOTE_INSTALL_RENEW_COMMAND.replacen(
            "trap - EXIT HUP INT TERM; claim_release",
            "printf 'replacement-owner\\n999999999\\n1\\n0\\n' > \"$lock/claim\"; trap - EXIT HUP INT TERM; claim_release",
            1,
        );
        assert_ne!(renew, REMOTE_INSTALL_RENEW_COMMAND);
        let mut command = local_shell_command("sh", &directory);
        command.args(["-c", &renew]);
        run_streaming_command(
            command,
            transaction.renewal_input(1),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(directory.join(".local/bin/.boomux.bootstrap.lock/claim")).unwrap(),
            "replacement-owner\n999999999\n1\n0\n"
        );
        fs::remove_file(directory.join(".local/bin/.boomux.bootstrap.lock/claim")).unwrap();
        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn zombie_claim_owner_does_not_block_watchdog_recovery() {
        let mut zombie = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let zombie_pid = i32::try_from(zombie.id()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let zombie_start = loop {
            let stat = fs::read_to_string(format!("/proc/{zombie_pid}/stat")).unwrap();
            let mut fields = stat.rsplit_once(") ").unwrap().1.split_whitespace();
            if fields.next().unwrap() == "Z" {
                break fields.nth(18).unwrap().to_owned();
            }
            assert!(Instant::now() < deadline, "child did not become a zombie");
            thread::sleep(Duration::from_millis(10));
        };

        let directory = runtime_directory();
        fs::create_dir_all(directory.join(".local/bin")).unwrap();
        let destination = directory.join(".local/bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let install = gate_watchdog(
            &REMOTE_INSTALL_COMMAND
                .replace("lease_limit=180", "lease_limit=1")
                .replace("[ \"$claim_age\" -ge 180 ]", "[ \"$claim_age\" -ge 1 ]"),
        );
        let transaction = run_local_upload_with_command(&directory, b"replacement", "sh", &install);
        let claim = directory.join(".local/bin/.boomux.bootstrap.lock/claim");
        fs::write(
            &claim,
            format!(
                "{}:{zombie_pid}:{zombie_start}\n{zombie_pid}\n{zombie_start}\n0\n",
                transaction.0
            ),
        )
        .unwrap();
        fs::write(directory.join("watchdog-tick"), b"").unwrap();
        let lock = directory.join(".local/bin/.boomux.bootstrap.lock");
        let deadline = Instant::now() + Duration::from_secs(3);
        while lock.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!lock.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        zombie.wait().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn two_bootstrap_sessions_share_atomic_remote_install_exclusion() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let home = runtime.join("remote-home");
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        let destination = home.join(".local/bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let ssh = runtime.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\n{CONTROL_MASTER_ONLY_SCRIPT}\nlast=\nfor arg do last=$arg; done\nexec env HOME={} /bin/sh -c \"$last\"\n",
                quote_posix_shell(home.to_str().unwrap())
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let first = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let second = BootstrapSession::open_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        let transaction = InstallTransactionId::generate();
        let output = run_streaming_command_capture(
            first.command(REMOTE_INSTALL_COMMAND),
            transaction.upload_input(b"first"),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            InstallTransactionId::parse_probe(&output.stdout).unwrap(),
            transaction
        );
        let second_transaction = InstallTransactionId::generate();
        let error = run_streaming_command_capture(
            second.command(REMOTE_INSTALL_COMMAND),
            second_transaction.upload_input(b"second"),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error_code(&error), "busy");
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        run_streaming_command(
            first.command(REMOTE_INSTALL_ROLLBACK_COMMAND),
            transaction.input(),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        drop(first);
        drop(second);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn rollback_watchdog_restores_after_local_status_is_lost() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join(".local/bin")).unwrap();
        let destination = directory.join(".local/bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let command_text = REMOTE_INSTALL_COMMAND.replacen("lease_limit=180", "lease_limit=1", 1);
        let transaction =
            run_local_upload_with_command(&directory, b"provisional", "sh", &command_text);
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        assert!(
            directory
                .join(".local/bin")
                .join(&transaction.0)
                .join("new")
                .exists()
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        let lock = directory.join(".local/bin/.boomux.bootstrap.lock");
        while lock.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        assert!(!lock.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn renewable_lease_survives_multiple_simulated_watchdog_windows() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join(".local/bin")).unwrap();
        let destination = directory.join(".local/bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let install = REMOTE_INSTALL_COMMAND.replace("lease_limit=180", "lease_limit=2");
        let transaction = run_local_upload_with_command(&directory, b"replacement", "sh", &install);
        for renewal in 1..=4 {
            thread::sleep(Duration::from_secs(1));
            let mut renew = Command::new("sh");
            renew
                .args(["-c", REMOTE_INSTALL_RENEW_COMMAND])
                .env("HOME", &directory);
            run_streaming_command(
                renew,
                transaction.renewal_input(renewal),
                Duration::from_secs(1),
            )
            .unwrap();
            assert_eq!(fs::read(&destination).unwrap(), b"previous");
        }
        run_local_activation(&directory, "sh", &transaction, RemoteInstallReason::Missing);
        run_local_transaction(&directory, REMOTE_INSTALL_COMMIT_COMMAND, &transaction);
        assert_eq!(fs::read(&destination).unwrap(), b"replacement");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn watchdog_honors_renewal_at_the_claim_boundary() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join(".local/bin")).unwrap();
        let destination = directory.join(".local/bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let install = REMOTE_INSTALL_COMMAND
            .replace("lease_limit=180", "lease_limit=1")
            .replacen(
                "if claim_acquire; then current=",
                ": > \"$HOME/watchdog-boundary\"; while [ ! -e \"$HOME/watchdog-release\" ]; do /bin/sleep 1; done; if claim_acquire; then current=",
                1,
            )
            .replacen(
                "lease_value=$claimed_lease; unchanged=0; continue",
                "lease_value=$claimed_lease; unchanged=0; : > \"$HOME/watchdog-honored\"; continue",
                1,
            );
        assert!(install.contains("watchdog-boundary"));
        assert!(install.contains("watchdog-honored"));
        let transaction = run_local_upload_with_command(&directory, b"replacement", "sh", &install);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !directory.join("watchdog-boundary").exists() {
            assert!(Instant::now() < deadline, "watchdog did not reach boundary");
            thread::sleep(Duration::from_millis(10));
        }
        let claim = directory.join(".local/bin/.boomux.bootstrap.lock/claim");
        fs::create_dir(&claim).unwrap();
        let mut blocked_renewal = Command::new("sh");
        blocked_renewal
            .args(["-c", REMOTE_INSTALL_RENEW_COMMAND])
            .env("HOME", &directory);
        assert!(
            run_streaming_command(
                blocked_renewal,
                transaction.renewal_input(1),
                Duration::from_secs(1),
            )
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(
                directory
                    .join(".local/bin")
                    .join(&transaction.0)
                    .join("lease")
            )
            .unwrap(),
            "0\n"
        );
        fs::remove_dir(&claim).unwrap();
        let mut renew = Command::new("sh");
        renew
            .args(["-c", REMOTE_INSTALL_RENEW_COMMAND])
            .env("HOME", &directory);
        run_streaming_command(renew, transaction.renewal_input(1), Duration::from_secs(1)).unwrap();
        fs::write(directory.join("watchdog-release"), b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !directory.join("watchdog-honored").exists() {
            assert!(
                Instant::now() < deadline,
                "watchdog did not honor boundary renewal"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        run_local_activation(&directory, "sh", &transaction, RemoteInstallReason::Missing);
        run_local_transaction(&directory, REMOTE_INSTALL_COMMIT_COMMAND, &transaction);
        assert_eq!(fs::read(&destination).unwrap(), b"replacement");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rollback_restores_daemon_consistency_for_restarted_and_first_installs() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join(".local/bin")).unwrap();
        let destination = directory.join(".local/bin/boomux");
        let log = directory.join("daemon-actions");
        fs::write(
            &destination,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        )
        .unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let transaction = run_local_install(&directory, b"provisional");
        fs::write(
            directory
                .join(".local/bin")
                .join(&transaction.0)
                .join("prior_daemon"),
            protocol::PROTOCOL_VERSION.to_string(),
        )
        .unwrap();
        fs::write(
            directory
                .join(".local/bin")
                .join(&transaction.0)
                .join("daemon_contacted"),
            b"",
        )
        .unwrap();
        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
        assert_eq!(fs::read_to_string(&log).unwrap(), "daemon restart\n");

        fs::remove_file(&destination).unwrap();
        let provisional = format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display());
        let transaction = run_local_install(&directory, provisional.as_bytes());
        fs::write(
            directory
                .join(".local/bin")
                .join(&transaction.0)
                .join("prior_daemon"),
            b"absent",
        )
        .unwrap();
        fs::write(
            directory
                .join(".local/bin")
                .join(&transaction.0)
                .join("daemon_contacted"),
            b"",
        )
        .unwrap();
        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
        assert!(!destination.exists());
        assert_eq!(fs::read_to_string(&log).unwrap(), "daemon restart\n");

        fs::write(
            &destination,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        )
        .unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let transaction = run_local_install(&directory, provisional.as_bytes());
        fs::write(
            directory
                .join(".local/bin")
                .join(&transaction.0)
                .join("prior_daemon"),
            b"absent",
        )
        .unwrap();
        fs::write(
            directory
                .join(".local/bin")
                .join(&transaction.0)
                .join("daemon_contacted"),
            b"",
        )
        .unwrap();
        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
        assert!(destination.exists());
        assert_eq!(fs::read_to_string(&log).unwrap(), "daemon restart\n");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rollback_remains_retryable_when_restored_daemon_restart_fails() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join(".local/bin")).unwrap();
        let destination = directory.join(".local/bin/boomux");
        fs::write(&destination, "#!/bin/sh\n[ ! -e \"$HOME/restart-fail\" ]\n").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let transaction = run_local_install(&directory, b"provisional");
        let transaction_dir = directory.join(".local/bin").join(&transaction.0);
        fs::write(
            transaction_dir.join("prior_daemon"),
            protocol::PROTOCOL_VERSION.to_string(),
        )
        .unwrap();
        fs::write(transaction_dir.join("daemon_contacted"), b"").unwrap();
        fs::write(directory.join("restart-fail"), b"").unwrap();

        let mut rollback = local_shell_command("sh", &directory);
        rollback.args(["-c", REMOTE_INSTALL_ROLLBACK_COMMAND]);
        run_streaming_command(rollback, transaction.input(), Duration::from_secs(1)).unwrap_err();
        assert!(transaction_dir.exists());
        assert!(directory.join(".local/bin/.boomux.bootstrap.lock").exists());
        assert!(transaction_dir.join("new").exists());

        fs::remove_file(directory.join("restart-fail")).unwrap();
        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
        assert!(!transaction_dir.exists());
        assert!(!directory.join(".local/bin/.boomux.bootstrap.lock").exists());
        assert!(
            fs::read_to_string(&destination)
                .unwrap()
                .contains("restart-fail")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rollback_does_not_signal_reused_watchdog_pid() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join(".local/bin")).unwrap();
        let destination = directory.join(".local/bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let transaction = run_local_install(&directory, b"provisional");
        let transaction_dir = directory.join(".local/bin").join(&transaction.0);
        let mut unrelated = Command::new("sleep").arg("60").spawn().unwrap();
        fs::write(
            transaction_dir.join("watchdog_pid"),
            format!("{}\n", unrelated.id()),
        )
        .unwrap();
        fs::write(
            transaction_dir.join("watchdog_start"),
            b"not-this-process\n",
        )
        .unwrap();

        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
        assert!(unrelated.try_wait().unwrap().is_none());
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn watchdog_retains_transaction_when_backup_restore_fails() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join(".local/bin")).unwrap();
        let destination = directory.join(".local/bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let install = gate_watchdog(
            &REMOTE_INSTALL_COMMAND
                .replace("lease_limit=180", "lease_limit=1")
                .replace(
                    "/bin/mv -f \"$transaction/backup\" \"$destination\" || return 1;",
                    "false || return 1;",
                ),
        );
        let transaction = run_local_upload_with_command(&directory, b"provisional", "sh", &install);
        run_local_activation(&directory, "sh", &transaction, RemoteInstallReason::Missing);
        fs::write(directory.join("watchdog-tick"), b"").unwrap();
        thread::sleep(Duration::from_secs(2));
        let transaction_dir = directory.join(".local/bin").join(&transaction.0);
        assert!(transaction_dir.exists());
        assert!(directory.join(".local/bin/.boomux.bootstrap.lock").exists());

        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pinned_source_bytes_survive_path_replacement() {
        let directory = runtime_directory();
        fs::create_dir_all(&directory).unwrap();
        let source_path = directory.join("boomux-source");
        fs::write(&source_path, b"authorized-bytes").unwrap();
        let bytes = read_bounded_file(&source_path).unwrap();
        let source = RemoteInstallSource::CurrentBinary {
            path: source_path.clone(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes,
        };
        fs::write(&source_path, b"replaced-after-confirmation").unwrap();
        assert_eq!(source.bytes(), b"authorized-bytes");
        assert!(source.description().contains("sha256"));
        let transaction = run_local_install(&directory, source.bytes());
        assert_eq!(
            fs::read(directory.join(".local/bin/boomux")).unwrap(),
            b"authorized-bytes"
        );
        run_local_transaction(&directory, REMOTE_INSTALL_ROLLBACK_COMMAND, &transaction);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn install_source_pinning_rejects_symlinks_and_special_files() {
        let directory = runtime_directory();
        fs::create_dir_all(&directory).unwrap();
        let regular = directory.join("regular");
        let symlink = directory.join("symlink");
        fs::write(&regular, b"boomux").unwrap();
        std::os::unix::fs::symlink(&regular, &symlink).unwrap();
        assert!(read_bounded_file(&symlink).is_err());
        assert!(read_bounded_file(&directory).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn install_source_pinning_rejects_same_length_in_place_mutation() {
        let directory = runtime_directory();
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("source");
        fs::write(&source, b"before!!").unwrap();
        let error = read_bounded_file_with_hook(&source, || {
            let mut file = OpenOptions::new().write(true).open(&source).unwrap();
            file.write_all(b"after!!!").unwrap();
            file.sync_all().unwrap();
        })
        .unwrap_err();
        assert_eq!(error_code(&error), "bootstrap_install_failed");
        assert!(error.to_string().contains("changed while"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_streamed_install_leaves_previous_binary_usable() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join("bin")).unwrap();
        let destination = directory.join("bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        let temporary = directory.join("bin/.boomux.test");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "set -eu; trap 'rm -f \"$TEMPORARY\"' EXIT; cat > \"$TEMPORARY\"; false; mv -f \"$TEMPORARY\" \"$DESTINATION\"",
        ]);
        command
            .env("TEMPORARY", &temporary)
            .env("DESTINATION", &destination);
        assert!(
            run_streaming_command(command, b"replacement".to_vec(), Duration::from_secs(1))
                .is_err()
        );
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        assert!(!temporary.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn release_checksum_requires_exact_asset_name_and_digest() {
        let directory = runtime_directory();
        fs::create_dir_all(&directory).unwrap();
        let archive = directory.join("asset.tar.gz");
        let checksum = directory.join("asset.tar.gz.sha256");
        fs::write(&archive, b"archive").unwrap();
        let digest = format!("{:x}", Sha256::digest(b"archive"));
        fs::write(&checksum, format!("{digest}  asset.tar.gz\n")).unwrap();
        verify_release_checksum(&archive, &checksum, "asset.tar.gz").unwrap();

        fs::write(&checksum, format!("{digest}  another.tar.gz\n")).unwrap();
        assert_eq!(
            verify_release_checksum(&archive, &checksum, "asset.tar.gz")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::write(&checksum, format!("{}  asset.tar.gz\n", "0".repeat(64))).unwrap();
        assert_eq!(
            verify_release_checksum(&archive, &checksum, "asset.tar.gz")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
