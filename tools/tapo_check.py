#!/usr/bin/env python3
"""
tapo_check.py -- is this the password a Tapo camera expects? (issue #74)

Asks the camera for the first step of its secure login (no password is
sent) and checks the typed password against the camera's answer
(device_confirm), the same way backend_daemon's tapo.rs does. Nothing here
is a login attempt: it never counts toward the camera's lockout.

    python3 tools/tapo_check.py 192.168.1.133
    python3 tools/tapo_check.py 192.168.1.133 --try-login

--try-login: if the camera's answer doesn't match (firmwares after TP-Link's
2026 device_confirm fix), make EXACTLY ONE real login attempt anyway (the
SHA-256 form). That one DOES count toward the camera's lockout (10 failed
logins = 30 minutes); it says whether the login itself still works.

The camera is asked over HTTPS without checking its certificate: it's
self-signed, and nothing secret is sent.
"""
import getpass
import hashlib
import json
import os
import ssl
import sys
import urllib.request


def ask(host, cnonce):
    body = json.dumps({"method": "login", "params": {"cnonce": cnonce, "encrypt_type": "3", "username": "admin"}}).encode()
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    request = urllib.request.Request(f"https://{host}/", data=body, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, context=context, timeout=10) as reply:
            return json.loads(reply.read())
    except urllib.error.HTTPError as error:
        return json.loads(error.read())


def try_login(host, cnonce, nonce, password):
    """One real login (digest_passwd, SHA-256 form) -- counts toward the lockout."""
    hashed = hashlib.sha256(password.encode()).hexdigest().upper()
    digest = hashlib.sha256((hashed + cnonce + nonce).encode()).hexdigest().upper()
    body = json.dumps({"method": "login", "params": {
        "cnonce": cnonce, "encrypt_type": "3", "username": "admin",
        "digest_passwd": digest + cnonce + nonce}}).encode()
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    request = urllib.request.Request(f"https://{host}/", data=body, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, context=context, timeout=10) as reply:
            answer = json.loads(reply.read())
    except urllib.error.HTTPError as error:
        answer = json.loads(error.read())
    result = answer.get("result", {})
    if "stok" in result:
        print(f"LOGIN WORKS (SHA-256): the camera gave a session (user_group {result.get('user_group')}).")
        print("-> only its first answer changed; the hub can skip that check.")
    else:
        data = result.get("data", {})
        print(f"Login refused: error {answer.get('error_code')}, code {data.get('code')}, "
              f"attempts {data.get('time')}/{data.get('max_time')}, locked {data.get('sec_left', 0)} s")


def main():
    host = sys.argv[1] if len(sys.argv) > 1 and not sys.argv[1].startswith("--") else "192.168.1.133"
    attempt = "--try-login" in sys.argv
    cnonce = os.urandom(8).hex().upper()
    answer = ask(host, cnonce)
    data = answer.get("result", {}).get("data", {})
    if data.get("sec_left", 0) > 0:
        sys.exit(f"The camera is locked for {data['sec_left']} s (too many wrong logins). Wait, then try again.")
    nonce, confirm = data.get("nonce"), data.get("device_confirm")
    if not nonce or not confirm:
        sys.exit(f"Not the expected answer: {answer}")
    print(f"camera answer: encrypt_type {data.get('encrypt_type')}, attempts counter {data.get('time')}/{data.get('max_time')}")
    while True:
        password = getpass.getpass("Password to check (not shown, Enter alone to stop): ")
        if not password:
            return
        sha = hashlib.sha256(password.encode()).hexdigest()
        md5 = hashlib.md5(password.encode()).hexdigest()
        # The known form (pytapo), then variations seen across firmwares.
        candidates = [
            ("SHA256", sha.upper()), ("MD5", md5.upper()),
            ("sha256 lower", sha), ("md5 lower", md5),
            ("SHA256(MD5)", hashlib.sha256(md5.upper().encode()).hexdigest().upper()),
            ("plain", password),
        ]
        hit = None
        for name, hashed in candidates:
            for upper in (True, False):
                digest = hashlib.sha256((cnonce + hashed + nonce).encode()).hexdigest()
                digest = digest.upper() if upper else digest
                if digest + nonce + cnonce == confirm:
                    hit = f"{name}, digest {'upper' if upper else 'lower'} case"
        if hit:
            print(f"Correct: this is the camera's password ({hit}).")
        else:
            print(f"Not the password this camera knows ({len(password)} characters typed).")
            if attempt:
                try_login(host, cnonce, nonce, password)
                return


if __name__ == "__main__":
    main()
