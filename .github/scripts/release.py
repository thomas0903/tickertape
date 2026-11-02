#!/usr/bin/env python3
"""Release orchestration using the Python standard library, Cargo and GitHub CLI."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import time
import tomllib
import urllib.error
import urllib.request


CRATES = ("tickertape", "tickertape-cli")
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def gh_json(*args):
    return json.loads(run("gh", *args))


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": "tickertape-release"})
    return urllib.request.urlopen(request, timeout=30)


def fetch_json(url):
    with fetch(url) as response:
        return json.load(response)


def version_for_tag(tag):
    if not re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag):
        raise ValueError("release tags must be stable versions in the form vX.Y.Z")
    return tag[1:]


def validate_versions(source, version):
    with (source / "Cargo.toml").open("rb") as file:
        workspace = tomllib.load(file)["workspace"]
    if workspace["package"]["version"] != version:
        raise ValueError("tag does not match the workspace package version")
    if workspace["dependencies"]["tickertape"]["version"] != version:
        raise ValueError("workspace tickertape dependency does not match the tag")
    for name in CRATES:
        with (source / "crates" / name / "Cargo.toml").open("rb") as file:
            package = tomllib.load(file)["package"]
        if package["name"] != name or package["version"] != {"workspace": True}:
            raise ValueError(f"{name} must inherit the workspace version")


def require_green_ci(runs, sha):
    matching = [run for run in runs if run["head_sha"] == sha
                and run["head_branch"] == "main" and run["event"] == "push"]
    latest = max(matching, key=lambda run: run["run_number"], default=None)
    if latest is None or latest["status"] != "completed" or latest["conclusion"] != "success":
        raise ValueError("the tag commit must have a successful CI push run on main; rerun after CI passes")


def release_notes(changelog, version):
    match = re.search(rf"^## \[{re.escape(version)}\][^\n]*\n(.*?)(?=^## \[|\Z)",
                      changelog, re.MULTILINE | re.DOTALL)
    if match is None:
        raise ValueError(f"CHANGELOG.md has no section for {version}")
    # Reference definitions belong to the whole changelog, not the final section.
    notes = re.split(r"^\[[^\]]+\]: ", match.group(1), maxsplit=1, flags=re.MULTILINE)[0].strip()
    return (notes + "\n\n```sh\ncargo add tickertape\ncargo install tickertape-cli --locked\n```\n\n"
            f"[API documentation](https://docs.rs/tickertape/{version}/tickertape/) · "
            "[CLI binary installation](https://github.com/thomas0903/tickertape/blob/main/docs/install.md)\n")


def prepare(args):
    source = args.source.resolve()
    version = version_for_tag(args.tag)
    validate_versions(source, version)
    sha = run("git", "rev-parse", "HEAD", cwd=source)
    if run("git", "rev-parse", f"{args.tag}^{{commit}}", cwd=source) != sha:
        raise ValueError("source checkout does not match the release tag")
    subprocess.run(["git", "merge-base", "--is-ancestor", sha, "origin/main"], cwd=source, check=True)
    repo = os.environ["GITHUB_REPOSITORY"]
    runs = gh_json("api", f"repos/{repo}/actions/workflows/ci.yml/runs?head_sha={sha}&per_page=100")
    require_green_ci(runs["workflow_runs"], sha)
    args.notes.write_text(release_notes((source / "CHANGELOG.md").read_text(), version))
    outputs = {"tag": args.tag, "version": version, "sha": sha}
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as file:
        for key, value in outputs.items():
            file.write(f"{key}={value}\n")
    print(json.dumps(outputs))


def published_source(name, version):
    """Return the published commit, or None for a version that does not exist."""
    try:
        metadata = fetch_json(f"https://crates.io/api/v1/crates/{name}/{version}")["version"]
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise
    if metadata["yanked"]:
        raise ValueError(f"{name} {version} is yanked; refusing to release it")
    with fetch(f"https://static.crates.io/crates/{name}/{name}-{version}.crate") as response:
        archive = response.read()
    if hashlib.sha256(archive).hexdigest() != metadata["checksum"]:
        raise ValueError(f"checksum mismatch for {name} {version}")
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as crate:
        vcs = json.load(crate.extractfile(f"{name}-{version}/.cargo_vcs_info.json"))
    if vcs["git"].get("dirty", False):
        raise ValueError(f"{name} {version} was published from a dirty checkout")
    return vcs["git"]["sha1"]


def publish(args):
    validate_versions(args.source, args.version)
    # Cargo waits for index availability before publishing the dependent CLI.
    for name in CRATES:
        existing = published_source(name, args.version)
        if existing is not None:
            if existing != args.sha:
                raise ValueError(f"{name} {args.version} already exists from a different commit")
            print(f"{name} {args.version} is already published from {args.sha}; keeping it")
            continue
        subprocess.run(["cargo", "publish", "--locked", "-p", name], cwd=args.source, check=True)


def package(args):
    version = version_for_tag(args.tag)
    if args.target not in TARGETS:
        raise ValueError("unsupported release target")
    name = f"tickertape-{args.tag}-{args.target}"
    binary = args.source / "target" / args.target / "release/tickertape"
    files = [("tickertape", binary.read_bytes(), 0o755)]
    for filename in ("LICENSE-MIT", "LICENSE-APACHE"):
        files.append((filename, (args.source / filename).read_bytes(), 0o644))
    readme = (f"tickertape {version} CLI for {args.target}\n\n"
              "Run ./tickertape --help, or copy tickertape to a directory on your PATH.\n"
              "Verify the archive against SHA256SUMS from the same GitHub release.\n"
              "Linux binaries require glibc 2.35 or later. macOS binaries require macOS 11 or later.\n"
              "The macOS archives are unsigned and are not notarized.\n\n"
              "Installation: https://github.com/thomas0903/tickertape/blob/main/docs/install.md\n"
              "Crash contract: https://github.com/thomas0903/tickertape/blob/main/docs/crash-contract.md\n")
    files.append(("README.txt", readme.encode(), 0o644))
    epoch = int(run("git", "show", "-s", "--format=%ct", "HEAD", cwd=args.source))
    args.output.mkdir(parents=True, exist_ok=True)
    destination = args.output / f"{name}.tar.gz"
    with destination.open("wb") as file:
        with gzip.GzipFile(filename="", fileobj=file, mode="wb", mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive:
                for filename, data, mode in files:
                    info = tarfile.TarInfo(f"{name}/{filename}")
                    info.size, info.mode, info.mtime = len(data), mode, epoch
                    archive.addfile(info, io.BytesIO(data))
    print(destination)


def github_release(args):
    repo = os.environ["GITHUB_REPOSITORY"]
    archives = sorted(args.artifacts.glob("*.tar.gz"))
    expected = {f"tickertape-{args.tag}-{target}.tar.gz" for target in TARGETS}
    if {path.name for path in archives} != expected:
        raise ValueError("expected exactly one CLI archive for each release target")
    checksums = args.artifacts / "SHA256SUMS"
    checksums.write_text("".join(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
                                 for path in archives))
    # List instead of treating every `release view` failure as a missing release.
    releases = gh_json("api", f"repos/{repo}/releases?per_page=100", "--paginate", "--slurp")
    existing = next((release for page in releases for release in page
                     if release["tag_name"] == args.tag), None)
    if existing is None:
        if not args.create:
            raise ValueError("binary-only runs require an existing GitHub release")
        subprocess.run(["gh", "release", "create", args.tag, "--repo", repo, "--verify-tag",
                        "--draft", "--title", f"tickertape {args.tag}",
                        "--notes-file", str(args.notes)], check=True)
        existing = gh_json("api", f"repos/{repo}/releases/tags/{args.tag}")
    asset_names = {asset["name"] for asset in existing["assets"]}
    for path in [*archives, checksums]:
        if path.name in asset_names:
            with tempfile.TemporaryDirectory() as temporary:
                subprocess.run(["gh", "release", "download", args.tag, "--repo", repo,
                                "--pattern", path.name, "--dir", temporary], check=True)
                if Path(temporary, path.name).read_bytes() != path.read_bytes():
                    raise ValueError(f"existing asset {path.name} differs; refusing to overwrite it")
            continue
        subprocess.run(["gh", "release", "upload", args.tag, str(path), "--repo", repo], check=True)
    if existing["draft"]:
        subprocess.run(["gh", "release", "edit", args.tag, "--repo", repo,
                        "--draft=false", "--latest"], check=True)


def verify(args):
    deadline = time.monotonic() + args.timeout
    pending = set(CRATES)
    while pending:
        for name in list(pending):
            try:
                source = published_source(name, args.version)
                if source is None:
                    continue
                if source != args.sha:
                    raise ValueError(f"published {name} source differs from the release tag")
                status = fetch_json(f"https://docs.rs/crate/{name}/{args.version}/status.json")
                if status.get("doc_status") is True and status.get("version") == args.version:
                    pending.remove(name)
                    print(f"{name} {args.version}: published source and docs.rs verified", flush=True)
            except urllib.error.HTTPError as error:
                if error.code not in (404, 429, 500, 502, 503, 504):
                    raise
            except (urllib.error.URLError, TimeoutError):
                pass
        if pending:
            if time.monotonic() >= deadline:
                raise TimeoutError(f"publication/docs.rs verification timed out: {sorted(pending)}")
            time.sleep(20)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    command = commands.add_parser("prepare")
    command.add_argument("--source", type=Path, required=True)
    command.add_argument("--tag", required=True)
    command.add_argument("--notes", type=Path, required=True)
    command.set_defaults(function=prepare)
    command = commands.add_parser("publish")
    command.add_argument("--source", type=Path, required=True)
    command.add_argument("--version", required=True)
    command.add_argument("--sha", required=True)
    command.set_defaults(function=publish)
    command = commands.add_parser("package")
    command.add_argument("--source", type=Path, required=True)
    command.add_argument("--tag", required=True)
    command.add_argument("--target", required=True)
    command.add_argument("--output", type=Path, required=True)
    command.set_defaults(function=package)
    command = commands.add_parser("github-release")
    command.add_argument("--tag", required=True)
    command.add_argument("--artifacts", type=Path, required=True)
    command.add_argument("--notes", type=Path, required=True)
    command.add_argument("--create", action="store_true")
    command.set_defaults(function=github_release)
    command = commands.add_parser("verify")
    command.add_argument("--version", required=True)
    command.add_argument("--sha", required=True)
    command.add_argument("--timeout", type=int, default=900)
    command.set_defaults(function=verify)
    args = parser.parse_args()
    args.function(args)


if __name__ == "__main__":
    main()
