#!/usr/bin/env python3
"""Reconstruct and stage the pinned local Google clients; never install them."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


KIT = Path(__file__).resolve().parent


def run(args, cwd=None, capture=False, env=None):
    return subprocess.run(
        args, cwd=cwd, check=True, text=True, env=env,
        stdout=subprocess.PIPE if capture else None,
    ).stdout


def apply(source, patch, directory=None):
    args = ["git", "apply"]
    if directory:
        args.append("--directory=" + directory)
    run(args + ["--check", str(KIT / patch)], cwd=source)
    run(args + [str(KIT / patch)], cwd=source)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="new staging directory; must not exist")
    parser.add_argument("--repository", help="optional local gogcli Git repository for offline fetch")
    parser.add_argument("--source-only", action="store_true", help="reconstruct sources without tests/builds")
    parser.add_argument("--sign", action="store_true", help="sign staged builds with the existing canonical launcher")
    args = parser.parse_args()
    if args.source_only and args.sign:
        parser.error("--sign requires a build")
    manifest = json.loads((KIT / "manifest.json").read_text())
    for name, digest in manifest["patch_sha256"].items():
        if hashlib.sha256((KIT / name).read_bytes()).hexdigest() != digest:
            raise RuntimeError("Patch checksum mismatch: " + name)
    if not args.source_only:
        if sys.platform != "darwin":
            raise RuntimeError("Native macOS is required for the Keychain build and regressions")
        if run(["go", "env", "GOVERSION"], capture=True).strip() != manifest["go_version"]:
            raise RuntimeError("Use the pinned Go toolchain: " + manifest["go_version"])
    output = args.output.expanduser().resolve()
    output.mkdir(parents=True, exist_ok=False)
    module = json.loads(run([
        "go", "mod", "download", "-json",
        manifest["keyring_module"] + "@" + manifest["keyring_version"],
    ], cwd=output, capture=True))
    if module.get("Sum") != manifest["keyring_sum"]:
        raise RuntimeError("Keyring module checksum mismatch")
    dependency = Path(module["Dir"])
    env = dict(os.environ, CGO_ENABLED="1")
    (output / "bin").mkdir()
    for component in ["gog", "gog-calendar-patch"]:
        source = output / (component + "-source")
        source.mkdir()
        run(["git", "init", "--quiet"], cwd=source)
        run(["git", "fetch", "--quiet", "--depth=1",
             args.repository or manifest["gogcli_repository"], manifest["gogcli_commit"]], cwd=source)
        run(["git", "checkout", "--quiet", "--detach", "FETCH_HEAD"], cwd=source)
        if run(["git", "rev-parse", "HEAD"], cwd=source, capture=True).strip() != manifest["gogcli_commit"]:
            raise RuntimeError("Unexpected gogcli source commit")
        target = source / "third_party" / "keyring"
        target.mkdir(parents=True)
        for path in dependency.iterdir():
            if path.is_file() and (path.suffix == ".go" or path.name in ["go.mod", "go.sum", "LICENSE", "README.md"]):
                shutil.copyfile(path, target / path.name)
        apply(source, "0003-keyring-errors.patch", "third_party/keyring")
        apply(source, "0001-shared-client.patch")
        if component == "gog-calendar-patch":
            apply(source, "0002-calendar-companion.patch")
        run(["git", "diff", "--check"], cwd=source)
        if args.source_only:
            continue
        # The upstream dependency's other tests can touch real Keychain state.
        run(["go", "test", "-run", "^TestKeychainQueryError$", "."], cwd=target, env=env)
        run(["go", "test", "./internal/secrets", "./internal/googleapi",
             "./internal/errfmt", "./internal/cmd"], cwd=source, env=env)
        version = ("v0.38.1-calendar-patch-guards-keychain-fix2" if component == "gog-calendar-patch"
                   else "v0.38.1-calendar-single-attempt-keychain-fix2")
        candidate = output / "bin" / component
        run(["go", "build", "-trimpath", "-buildvcs=false", "-ldflags",
             "-X github.com/openclaw/gogcli/internal/cmd.version=" + version,
             "-o", str(candidate), "./cmd/gog"], cwd=source, env=env)
        if args.sign:
            launcher = Path.home() / ".zeroclaw/bin/zeroclaw-signed-launch"
            run([str(launcher), component, "--sign-only", str(candidate)])
        print("Staged " + str(candidate), flush=True)
    print("Sources and candidates are staged. No installed executable or credential was changed.")


if __name__ == "__main__":
    main()
