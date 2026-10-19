#!/usr/bin/env python3
"""Hermetic end-to-end harness: drives the REAL chibi-tui binary through the
full missing-backend onboarding flow under a pseudo-terminal (PTY).

Adapted from the tui_scheduler_compat e2e harness pattern (JSONL stdio client
against a deterministic backend), but here the client is the actual TUI
process speaking ratatui/crossterm over a PTY, exercising the user-visible
flow: splash → setup screen (missing backend) → probe → one-click install →
backend config screen → save → respawn → handshake → live chat turn → quit.

Two modes:

* ``fake`` (default), fully OFFLINE and hermetic: fake ``python3`` and
  ``pipx`` executables on a prepended PATH simulate the probed environment
  (interpreter by execution, PIPX_BIN_DIR query, instant "install"), and the
  installed ``chibi`` is a wrapper around the repo's deterministic
  ``tests/fake_backend.py``. All state (HOME, storage root, settings file)
  lives in a temp tree; the real ``~/chibi-bot/settings`` is never touched
  (``CHIBI_TUI_SETTINGS_PATH`` override, the seam both config shells honor).
* ``live``: REAL smoke on the owner machine: no fake probes; the TUI probes
  the machine's real interpreters and performs a REAL ``pipx install
  chibi-bot`` from PyPI, isolated via temp ``PIPX_HOME``/``PIPX_BIN_DIR``
  and a temp ``HOME``, so nothing outside the temp tree is written. The
  agent turn is NOT attempted in live mode (needs API keys): the fake-mode
  chat turn covers the request pipeline end to end (same precedent as the
  scheduler e2e, which verified a real agent turn against a mock LLM).

Usage:
    python3 tests/e2e_onboarding_harness.py [--mode fake|live]
                                            [--bin target/debug/chibi-tui]

Exit code 0 = all assertions passed. Artifacts: <tmp>/pty.log (raw PTY
transcript) and <tmp>/tui_stderr.log.
"""

import argparse
import fcntl
import json
import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


# --- PTY driving helpers -----------------------------------------------------


class PtySession:
    def __init__(self, argv, env, cwd, tmp, rows=40, cols=120):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.transcript = open(os.path.join(tmp, "pty.log"), "wb")
        self.stderr_log = open(os.path.join(tmp, "tui_stderr.log"), "wb")
        self.proc = subprocess.Popen(
            argv,
            stdin=slave,
            stdout=slave,
            stderr=self.stderr_log,
            env=env,
            cwd=cwd,
            start_new_session=True,
        )
        os.close(slave)
        self.buf = b""
        # Expectations search ONLY unconsumed output: the same title renders
        # on both the pre-loop config screen (Shell A) and the auto-opened
        # in-app editor (Shell B), so a full-buffer search would match the
        # first occurrence forever.
        self.mark = 0

    def send(self, data: bytes):
        os.write(self.master, data)

    def read_available(self, timeout: float) -> bytes:
        r, _, _ = select.select([self.master], [], [], timeout)
        if not r:
            return b""
        try:
            chunk = os.read(self.master, 65536)
        except OSError:
            return b""
        # crossterm asks the terminal for the cursor position (ESC[6n) when
        # entering the alternate screen; a real terminal replies. Emulate it
        # or the TUI aborts with "cursor position could not be read".
        if b"\x1b[6n" in chunk:
            try:
                os.write(self.master, b"\x1b[1;1R")
            except OSError:
                pass
        return chunk

    def expect(self, needle: str, timeout: float) -> bool:
        """Read until `needle` appears AFTER the last consumed marker."""
        deadline = time.time() + timeout
        needle_b = needle.encode("utf-8")
        while time.time() < deadline:
            idx = self.buf.find(needle_b, self.mark)
            if idx != -1:
                self.mark = idx + len(needle_b)
                return True
            chunk = self.read_available(0.2)
            if chunk:
                self.buf += chunk
                self.transcript.write(chunk)
                self.transcript.flush()
            if not self.alive():
                return self.buf.find(needle_b, self.mark) != -1
        return False

    def alive(self) -> bool:
        return self.proc.poll() is None

    def wait(self, timeout: float) -> int:
        try:
            return self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            return self.proc.wait()

    def close(self):
        for f in (self.transcript, self.stderr_log):
            try:
                f.close()
            except OSError:
                pass
        try:
            os.close(self.master)
        except OSError:
            pass


# --- fake environment builders (offline mode) --------------------------------


def real_python3() -> str:
    p = shutil.which("python3")
    if not p:
        raise SystemExit("python3 not found on the harness PATH")
    return os.path.realpath(p)


def write_fake_python(path: str, real_py: str):
    body = f"""#!{real_py}
import os, sys
args = sys.argv[1:]
if "-c" in args:
    code = args[args.index("-c") + 1] if len(args) > args.index("-c") + 1 else ""
    # Version probe: import sys;print('%d.%d.%d' % sys.version_info[:3])
    if "sys.version_info" in code:
        print("3.12.4"); sys.exit(0)
    # PEP 668 marker probe: report NOT externally managed
    if "EXTERNALLY-MANAGED" in code:
        print("False"); sys.exit(0)
if args[:2] == ["-m", "venv"]:
    os.makedirs(args[2], exist_ok=True); sys.exit(0)
if args[:2] == ["-m", "pip"]:
    print("fake pip: ok"); sys.exit(0)
os.execv({real_py!r}, [{real_py!r}] + args)
"""
    with open(path, "w") as f:
        f.write(body)
    os.chmod(path, 0o755)


def fake_chibi_body(real_py: str, fake_backend: str) -> str:
    return f"""#!/bin/sh
# fake installed `chibi`: `stdio --help` satisfies the verification probe,
# `stdio --tui` runs the deterministic fake backend from tests/fake_backend.py.
if [ "$1" = "stdio" ] && [ "$2" = "--help" ]; then
  echo "chibi stdio --tui (fake)"
  exit 0
fi
WS=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--workspace" ]; then WS="$a"; fi
  prev="$a"
done
exec {real_py!r} {fake_backend!r} --workspace "$WS"
"""


def write_fake_pipx(path: str, pipx_bin_dir: str, chibi_body: str):
    body = f"""#!{real_python3()}
import os, sys
args = sys.argv[1:]
BIN_DIR = {pipx_bin_dir!r}
if args == ["--version"]:
    print("1.6.0-fake"); sys.exit(0)
if args[:2] == ["environment", "--value"] and len(args) > 2 and args[2] == "PIPX_BIN_DIR":
    print(BIN_DIR); sys.exit(0)
if args and args[0] == "install":
    assert "chibi-bot" in args, "TUI must install chibi-bot, got %r" % (args,)
    os.makedirs(BIN_DIR, exist_ok=True)
    chibi = os.path.join(BIN_DIR, "chibi")
    with open(chibi, "w") as f:
        f.write({chibi_body!r})
    os.chmod(chibi, 0o755)
    print("fake pipx: installed chibi-bot"); sys.exit(0)
print("unexpected pipx call: %r" % (args,), file=sys.stderr)
sys.exit(1)
"""
    with open(path, "w") as f:
        f.write(body)
    os.chmod(path, 0o755)


# --- harness -----------------------------------------------------------------


def build_env(tmp: str, mode: str) -> tuple[dict, str, str]:
    home = os.path.join(tmp, "home")
    tui_home = os.path.join(tmp, "tui-home")
    fakebin = os.path.join(tmp, "fakebin")
    pipx_bin = os.path.join(tmp, "pipx-bin")
    for d in (home, tui_home, fakebin, pipx_bin):
        os.makedirs(d, exist_ok=True)

    env = dict(os.environ)
    env["HOME"] = home
    env["CHIBI_TUI_HOME"] = tui_home
    # The settings-path test seam both config shells honor; it keeps the real
    # ~/chibi-bot/settings untouched BY CONSTRUCTION.
    env["CHIBI_TUI_SETTINGS_PATH"] = os.path.join(home, "chibi-bot", "settings")
    # MUST be absent (an empty value would route the pipeline into fake-backend
    # mode / read as a set override): the TUI must spawn `chibi` itself.
    env.pop("CHIBI_FAKE_BACKEND", None)
    env.pop("CHIBI_BACKEND_BIN", None)
    env.pop("CHIBI_ENV", None)

    if mode == "fake":
        real_py = real_python3()
        write_fake_python(os.path.join(fakebin, "python3"), real_py)
        write_fake_pipx(
            os.path.join(fakebin, "pipx"),
            pipx_bin,
            fake_chibi_body(real_py, os.path.join(REPO, "tests", "fake_backend.py")),
        )
        # Minimal PATH: the fake tools first, then only system dirs. The
        # developer's own `chibi` (backend venv) must NOT be resolvable;
        # the whole point is the missing-backend onboarding.
        env["PATH"] = os.pathsep.join(
            [fakebin, "/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        )
    else:  # live
        # Real probes, real PyPI install, isolated via PIPX_HOME/PIPX_BIN_DIR
        # and a temp HOME. pipx is exposed through a private wrapper (the
        # developer's pyenv shims also expose a real `chibi`, which must stay
        # out of reach so the onboarding actually triggers).
        env["PIPX_HOME"] = os.path.join(tmp, "pipx-home")
        env["PIPX_BIN_DIR"] = pipx_bin
        real_pipx = subprocess.run(
            ["pyenv", "which", "pipx"], capture_output=True, text=True
        ).stdout.strip() or shutil.which("pipx")
        if not real_pipx or not os.path.exists(real_pipx):
            raise SystemExit("live mode: real pipx not found (pyenv which pipx)")
        wrapper_dir = os.path.join(tmp, "pipx-wrapper")
        os.makedirs(wrapper_dir, exist_ok=True)
        wrapper = os.path.join(wrapper_dir, "pipx")
        with open(wrapper, "w") as f:
            f.write(f"""#!/bin/sh
# isolated-pipx wrapper: PIPX_HOME/PIPX_BIN_DIR already point into the
# harness temp tree; everything else is the REAL pipx.
exec {real_pipx!r} "$@"
""")
        os.chmod(wrapper, 0o755)
        live_dirs = [
            wrapper_dir,
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]
        # Belt and braces: drop any directory that actually provides `chibi`.
        for d in list(live_dirs):
            if os.path.exists(os.path.join(d, "chibi")):
                live_dirs.remove(d)
        env["PATH"] = os.pathsep.join(live_dirs)

    return env, tui_home, pipx_bin


def run_flow(bin_path: str, mode: str) -> list[str]:
    checks: list[str] = []
    tmp = tempfile.mkdtemp(prefix=f"chibi-tui-e2e-{mode}-")
    env, tui_home, pipx_bin = build_env(tmp, mode)

    sess = PtySession([bin_path], env, tmp, tmp)
    ok = True
    try:
        # splash holds ~500ms then auto-advances; wait it out.
        time.sleep(1.2)

        # 1. Backend missing → setup screen (not the generic error popup).
        if not sess.expect("not found on PATH", 20):
            raise AssertionError("setup screen did not appear for a missing backend")
        checks.append("setup screen shown for missing backend")

        # 2. Probe completes → offer of the first ladder rung (pipx).
        if not sess.expect("One-click install (pipx)", 90):
            raise AssertionError("offer for the pipx rung did not appear")
        checks.append("probe found interpreter+pipx, offered the 'pipx' rung")

        # 3. One-click install (fake: instant wrapper drop; live: real pipx).
        sess.send(b"i")
        install_timeout = 600 if mode == "live" else 60
        if not sess.expect("installed and verified", install_timeout):
            raise AssertionError("install did not reach the verified state")
        checks.append("install completed and binary verified via `stdio --help`")

        # 4. Unconditional handoff to the backend config screen (Shell A).
        if not sess.expect("Backend configuration", 20):
            raise AssertionError("config screen did not open after a verified install")
        checks.append("config screen opened after install (BYOK handoff)")
        time.sleep(0.3)
        settings_path = env["CHIBI_TUI_SETTINGS_PATH"]
        before = open(settings_path).read() if os.path.exists(settings_path) else None

        # 5. Make the doc dirty: CoreBehaviour group (Tab x3), toggle the
        #    first Bool row (FILESYSTEM_ACCESS), then Esc → save/discard
        #    modal → `s` = save-and-close → Flow::Saved.
        sess.send(b"\t\t\t")
        time.sleep(0.3)
        sess.send(b"\r")  # toggle FILESYSTEM_ACCESS
        time.sleep(0.3)
        sess.send(b"\x1b")  # Esc on a dirty screen → save/discard modal
        if not sess.expect("d discard", 10):
            raise AssertionError("discard/save modal did not appear after Esc on a dirty screen")
        sess.send(b"s")  # save & close
        after = None
        deadline = time.time() + 15
        while time.time() < deadline:
            if os.path.exists(settings_path):
                after = open(settings_path).read()
                if before is None or after != before:
                    break
            time.sleep(0.2)
        if after is None or after == before:
            raise AssertionError("saving from the config screen never wrote the settings file")
        if "FILESYSTEM_ACCESS=" not in after:
            raise AssertionError(f"saved settings lack the toggled key; got: {after[:400]!r}")
        checks.append("config save wrote the settings file (temp HOME, real one untouched)")

        # 6. Respawn via the persisted store → backend handshake → main loop.
        #    NOTE: the in-app config editor (Shell B) auto-opens only when NO
        #    provider key is "configured", but the generated template ships
        #    CUSTOMOPENAI_URL=http://localhost:1234/v1 ACTIVE (registry group
        #    ProviderKeys), so on a fresh generated config the auto-open is
        #    skipped (recorded as a residual in the task report). Handle both.
        if sess.expect("Backend configuration", 5):
            sess.send(b"\x1b")  # clean screen → close
        if not sess.expect("F1 help", 60):
            raise AssertionError("chat UI did not appear after the respawn handshake")
        checks.append("backend respawned via persisted path, handshake OK, chat UI up")

        # 7. One agent turn end-to-end (fake backend answers deterministically).
        if mode == "fake":
            sess.send(b"hi")
            time.sleep(0.2)
            sess.send(b"\r")  # Enter submits
            if not sess.expect("returns the integer", 30):
                raise AssertionError("fake backend result never rendered in the chat")
            checks.append("agent turn: request → backend → result bubble rendered")

        # 8. Quit: Ctrl+C → shared confirm → `y`. A busy-path Ctrl+C only
        #    cancels (never opens the popup), so retry until the confirm
        #    shows up.
        for _ in range(3):
            sess.send(b"\x03")
            if sess.expect("Quit chibi-tui?", 6):
                break
        else:
            raise AssertionError("quit confirmation did not appear on Ctrl+C")
        sess.send(b"y")
        rc = sess.wait(20)
        if rc != 0:
            raise AssertionError(f"TUI exit code {rc}, expected 0")
        checks.append("clean quit (exit 0)")

        # 9. Persisted store written by the installer flow.
        store = os.path.join(tui_home, "backend.json")
        if not os.path.exists(store):
            raise AssertionError(f"persisted backend store missing at {store}")
        doc = json.load(open(store))
        expected_bin = os.path.realpath(os.path.join(pipx_bin, "chibi"))
        # macOS /var → /private/var: the real pipx reports the canonical path.
        if os.path.realpath(doc.get("backend_path", "")) != expected_bin:
            raise AssertionError(
                f"store points at {doc.get('backend_path')!r}, expected {expected_bin!r}"
            )
        checks.append(f"persisted store points at the installed binary ({expected_bin})")

    except AssertionError as e:
        ok = False
        print(f"FAIL: {e}", file=sys.stderr)
        tail = sess.buf[-4000:].decode("utf-8", "replace")
        print(f"--- last PTY output ---\n{tail}", file=sys.stderr)
    finally:
        if sess.alive():
            try:
                os.killpg(sess.proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        sess.close()
        print(f"artifacts: {tmp}/pty.log, {tmp}/tui_stderr.log", file=sys.stderr)

    if not ok:
        raise SystemExit(1)
    return checks


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", choices=["fake", "live"], default="fake")
    ap.add_argument("--bin", default=os.path.join(REPO, "target", "debug", "chibi-tui"))
    args = ap.parse_args()

    if not os.path.exists(args.bin):
        raise SystemExit(f"TUI binary not found at {args.bin}; run `cargo build` first")

    print(f"mode={args.mode} bin={args.bin}")
    checks = run_flow(args.bin, args.mode)
    print("PASS, verified:")
    for c in checks:
        print(f"  [ok] {c}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
