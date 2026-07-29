#!/usr/bin/env python3
"""
Automated QEMU boot smoke test for the universal-controller-image.

Purpose: catch BSP/rootfs-level breakage (bad boot, broken systemd units,
sshd not coming up) in minutes, before anyone spends a hardware flash/boot
cycle on the STM32MP157F-DK2 finding the same problem the hard way.

This intentionally only tests what actually exists today: the Linux rootfs
booting under QEMU. It does NOT test RPMSG/OpenAMP or the M4 firmware -
qemuarm64 is a generic virtual ARM64 machine, it has no M4 coprocessor to
emulate. Once firmware_m4/ and linux_a7/ exist, extend the CHECKS list below
(and the hardware side gets its own smoke test against the real remoteproc).

Must be run from inside yocto_layers/ (matches run_build.sh / run_qemu.sh
convention). Use ./test_qemu.sh instead of calling this directly - it
handles the host-vs-container routing this script assumes it's already
inside the container for.

Exit code: 0 if every check passes, 1 otherwise.
"""
import argparse
import os
import re
import sys

try:
    import pexpect
except ImportError:
    print("FATAL: python3-pexpect is not installed. It's in the Dockerfile's "
          "apt list (python3-pexpect) - if you're seeing this, the container "
          "image is out of sync with the Dockerfile.", file=sys.stderr)
    sys.exit(2)

BUILD_DIR = "build-qemu"
TARGET_MACHINE = "qemuarm64"
# Must stay in sync with TARGET_IMAGE in run_build.sh / run_qemu.sh.
TARGET_IMAGE = "universal-controller-image"

# Unique-enough marker that we set as PS1 once logged in, so every later
# expect() is matching one deterministic string instead of guessing at
# whatever the image's default prompt looks like.
SHELL_PROMPT = "SMOKETEST# "

# Boot under TCG (no /dev/kvm) is much slower than accelerated QEMU - this
# is deliberately generous. Override with --boot-timeout if your machine is
# slower still.
DEFAULT_BOOT_TIMEOUT = 600
DEFAULT_CMD_TIMEOUT = 60
DEFAULT_SHUTDOWN_TIMEOUT = 120

# Patterns that mean "stop waiting, this boot has already failed" - checked
# alongside the login prompt so a bad boot fails fast instead of burning the
# full boot timeout before anyone notices.
BOOT_FAILURE_PATTERNS = [
    r"Kernel panic",
    r"FATAL:",
    r"Unable to find",          # runqemu couldn't locate the built image
    r"command not found",
]


class SmokeTestError(Exception):
    """Raised for any failure that should abort the run early with a clear reason."""


def run_cmd(child, cmd, prompt=SHELL_PROMPT, timeout=None):
    """Send a command over the console and return its output, prompt stripped."""
    if timeout is None:
        timeout = DEFAULT_CMD_TIMEOUT  # looked up at call time so --cmd-timeout can override it
    child.sendline(cmd)
    child.expect(re.escape(prompt), timeout=timeout)
    output = child.before
    # First line of `before` is the local-echoed command itself; drop it.
    lines = output.splitlines()
    if lines and cmd.strip() in lines[0]:
        lines = lines[1:]
    return "\n".join(lines).strip()


def wait_for_boot(child, results, boot_timeout):
    print(f"[*] Waiting for boot (timeout {boot_timeout}s, no /dev/kvm means "
          f"this runs under slow TCG emulation) ...")
    patterns = BOOT_FAILURE_PATTERNS + [r"login:"]
    try:
        idx = child.expect(patterns, timeout=boot_timeout)
    except pexpect.TIMEOUT:
        raise SmokeTestError(
            f"Boot did not reach a login prompt within {boot_timeout}s. "
            f"See the console log for where it stalled.")
    except pexpect.EOF:
        raise SmokeTestError(
            "QEMU process exited before reaching a login prompt (crashed or "
            "runqemu itself failed to launch). See the console log.")

    if idx < len(BOOT_FAILURE_PATTERNS):
        raise SmokeTestError(
            f"Boot failure pattern matched: {patterns[idx]!r}. "
            f"See the console log for context.")

    results.append(("Kernel boots to login prompt", True, ""))


def login(child, results):
    child.sendline("root")
    # debug-tweaks (enabled for this build dir only, see
    # build-qemu/conf/local.conf) means root has an empty password, but
    # different image configs still prompt for it - handle both.
    idx = child.expect(["Password:", r"[#\$]\s*$"], timeout=30)
    if idx == 0:
        child.sendline("")
        child.expect(r"[#\$]\s*$", timeout=30)
    results.append(("Root login succeeds", True, ""))

    # Pin a known prompt so every later expect() is unambiguous. Split the
    # literal in two and let the shell concatenate the quoted halves back
    # together (adjacent-string concatenation is standard sh/bash behaviour)
    # rather than sending SHELL_PROMPT verbatim: with local echo on, the
    # command we type is itself echoed back over the wire, and if that
    # echoed line already contained "SMOKETEST# " the expect() below would
    # match the echo of our own command instead of the real prompt that's
    # about to be printed - shifting every check that follows by one command.
    mid = len(SHELL_PROMPT) // 2
    part_a, part_b = SHELL_PROMPT[:mid], SHELL_PROMPT[mid:]
    child.sendline(f'export PS1="{part_a}""{part_b}"')
    child.expect(re.escape(SHELL_PROMPT), timeout=15)


def check_system_running(child, results):
    out = run_cmd(child, "systemctl is-system-running; echo RC=$?")
    ok = "RC=0" in out or "degraded" in out  # degraded = non-fatal unit failures, flagged separately below
    results.append(("systemd reports running/degraded (not failed to start)", ok, out.splitlines()[-1] if out else ""))


def check_failed_units(child, results):
    out = run_cmd(child, "systemctl --failed --no-legend | wc -l")
    try:
        failed_count = int(out.strip().splitlines()[-1])
    except (ValueError, IndexError):
        results.append(("No failed systemd units", False, f"couldn't parse count: {out!r}"))
        return
    results.append(("No failed systemd units", failed_count == 0, f"{failed_count} failed unit(s)"))
    if failed_count > 0:
        detail = run_cmd(child, "systemctl --failed --no-legend")
        print(f"    Failed units:\n{detail}")


def check_sshd(child, results):
    out = run_cmd(child, "systemctl is-active sshd 2>/dev/null || systemctl is-active ssh 2>/dev/null || echo not-found")
    active = "active" == out.strip().splitlines()[-1].strip()
    results.append(("sshd is active (IMAGE_FEATURES ssh-server-openssh)", active, out.strip()))


def check_kernel_version(child, results):
    out = run_cmd(child, "uname -a")
    ok = bool(out.strip())
    results.append(("Kernel reports a version (uname -a)", ok, out.strip()))


def check_rootfs_writable(child, results):
    out = run_cmd(child, "touch /tmp/.smoketest_write && echo WRITE_OK || echo WRITE_FAIL")
    ok = "WRITE_OK" in out
    results.append(("Rootfs is writable (/tmp)", ok, out.strip()))


def check_dmesg_for_errors(child, results):
    # Non-fatal signal, not a hard pass/fail: surfaces things worth a human
    # look without failing the whole run on every benign firmware-missing
    # warning that's normal on qemuarm64.
    out = run_cmd(child, "dmesg 2>/dev/null | grep -iE 'error|fail' | wc -l")
    try:
        count = int(out.strip().splitlines()[-1])
    except (ValueError, IndexError):
        count = -1
    detail = f"{count} dmesg lines matched error/fail (informational, not a failure gate)"
    print(f"[i] {detail}")


def shutdown(child, shutdown_timeout):
    print("[*] Sending poweroff and waiting for clean shutdown ...")
    try:
        child.sendline("poweroff")
        child.expect(pexpect.EOF, timeout=shutdown_timeout)
        return True
    except pexpect.TIMEOUT:
        print("[!] QEMU did not exit after poweroff within "
              f"{shutdown_timeout}s - killing it.", file=sys.stderr)
        child.close(force=True)
        return False


def main():
    global DEFAULT_CMD_TIMEOUT

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--boot-timeout", type=int, default=DEFAULT_BOOT_TIMEOUT)
    parser.add_argument("--cmd-timeout", type=int, default=DEFAULT_CMD_TIMEOUT)
    parser.add_argument("--shutdown-timeout", type=int, default=DEFAULT_SHUTDOWN_TIMEOUT)
    parser.add_argument("--log-file", default="qemu_smoke_test_console.log",
                         help="Full raw console transcript, written regardless of pass/fail.")
    args = parser.parse_args()

    DEFAULT_CMD_TIMEOUT = args.cmd_timeout

    if not os.path.isdir("poky"):
        print("FATAL: run this from yocto_layers/ (poky/ not found in cwd). "
              "Use ./test_qemu.sh instead of calling this script directly.",
              file=sys.stderr)
        sys.exit(2)

    results = []
    boot_cmd = (
        "bash -c \"set +u; source poky/oe-init-build-env "
        f"{BUILD_DIR} > /dev/null; set -u; "
        f"runqemu {TARGET_MACHINE} {TARGET_IMAGE} slirp nographic\""
    )

    print("=" * 79)
    print(f"QEMU smoke test - machine={TARGET_MACHINE} image={TARGET_IMAGE}")
    print("=" * 79)

    child = pexpect.spawn("/bin/bash", ["-c", boot_cmd], timeout=args.cmd_timeout,
                           encoding="utf-8", codec_errors="replace")
    log_f = open(args.log_file, "w")
    child.logfile = log_f

    overall_ok = True
    try:
        wait_for_boot(child, results, args.boot_timeout)
        login(child, results)
        check_kernel_version(child, results)
        check_rootfs_writable(child, results)
        check_system_running(child, results)
        check_failed_units(child, results)
        check_sshd(child, results)
        check_dmesg_for_errors(child, results)
        clean_shutdown = shutdown(child, args.shutdown_timeout)
        results.append(("Clean shutdown (poweroff -> QEMU exits)", clean_shutdown, ""))
    except SmokeTestError as e:
        print(f"\n[!] ABORTED: {e}", file=sys.stderr)
        overall_ok = False
        child.close(force=True)
    except (pexpect.TIMEOUT, pexpect.EOF) as e:
        # Belt-and-braces: any expect() we didn't explicitly wrap (e.g. inside
        # login() or a check_* helper) lands here instead of crashing out with
        # a raw traceback. Whatever checks already ran are still reported.
        kind = "timed out" if isinstance(e, pexpect.TIMEOUT) else "hit EOF (process exited unexpectedly)"
        print(f"\n[!] ABORTED: console interaction {kind} outside an expected "
              f"step. See {args.log_file} for the raw transcript.", file=sys.stderr)
        overall_ok = False
        child.close(force=True)
    finally:
        log_f.close()

    print("\n" + "=" * 79)
    print("SMOKE TEST SUMMARY")
    print("=" * 79)
    for name, ok, detail in results:
        status = "PASS" if ok else "FAIL"
        line = f"[{status}] {name}"
        if detail:
            line += f"  ({detail})"
        print(line)
        overall_ok = overall_ok and ok

    print("=" * 79)
    print(f"Full console transcript: {args.log_file}")
    print("RESULT: " + ("PASS" if overall_ok else "FAIL"))
    print("=" * 79)

    sys.exit(0 if overall_ok else 1)


if __name__ == "__main__":
    main()
