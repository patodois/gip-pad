#!/usr/bin/env python3
"""Public-tree gate: keep game files, decompiled code and secrets out of Git.

Hard rule of these repositories: game DATA is read at run time from the user's own install and is never committed,
and game CODE is never committed at all. This script rejects what breaks that rule, even when somebody uses
`git add -f`, and it can audit everything already in history.

The idea (reject private artifacts by path, in the working tree, in all of history, in a pushed range or from a
pre-push hook) follows the `tools/check_public_tree.py` of Horizon (MIT). This file is our own code; it shares
nothing with that script but the idea and the mode names.

Python 3.9+, standard library only. Needs `git` on PATH. Read-only: it never writes to the repository.

Modes (default: the files tracked right now, i.e. the index):
  --history           every commit reachable from any ref (and HEAD), merges diffed against each parent
  --range SPEC        the commits a `git rev-list SPEC` selects, e.g. `BASE..HEAD`; a bare `SHA` means all of
                      SHA's history. This is what CI runs on a push or a pull request.
  --pre-push          the ref updates Git writes on stdin of a pre-push hook; an empty push passes
Other options: --repo DIR, --config FILE, --allow FILE, --show-allowed, --json.
Exit status: 0 clean, 1 something private found, 2 the gate itself could not run (it fails closed).

Install as a hook (once per clone):  printf '#!/bin/sh\\nexec python3 tools/check_public_tree.py --pre-push\\n' \\
  > .git/hooks/pre-push && chmod +x .git/hooks/pre-push

Rules (ids as printed):
  private-root       a top-level directory that only holds local files (.local, decompiled, run, logs, ...)
  private-dir        a directory at any depth that holds decompiler or game output (decompiled, jadx-out, ...)
  private-extension  game archives and files (.vromfs.bin, .grp, .toc, .assets, .resS, .jar, .class, .mca, ...)
  private-name       exact game file names (level.dat, oo2core*.dll, Assets.zip, ...)
  compiled-binary    executables and libraries (.exe, .dll, .so, .dylib, ...): game CODE or build output
  foreign-source     .java/.cs/.smali files outside the globs the repository declares as its own (allow_source)
  secret-file        .env, SSH keys, keystores, cargo credentials
  large-binary / large-text   a blob over the size limit (LFS pointers count by the size they declare)
  decompiler-marker  the banner or comments a decompiler (CFR, Fernflower, JADX, ILSpy, ...) puts in its output
  game-package-decl  a Java file that declares a package inside the game (net.minecraft.*, com.mojang.*)
  foreign-copyright  a copyright holder that is neither us nor named in NOTICE / THIRD_PARTY_NOTICES
  secret-content     crates.io and GitHub tokens, private keys, cloud and API keys (never printed in full)

Per-repository extension: `.public-tree.cfg` (one directive per line, `#` starts a comment):
  private_root NAME | private_dir NAME | private_ext .EXT | private_name GLOB | private_path GLOB
  source_ext .EXT        another extension that counts as foreign source
  allow_source GLOB      source files that are ours (e.g. src/main/java/** in a mod)
  max_binary_bytes N | max_text_bytes N
  own_holder NAME        a copyright holder that is us
  notice_holder NAME     a third party whose licence text ships in the repository (e.g. next to the code it covers)
  marker REGEX           another decompiler marker
Lists only grow: a config can add rules, never remove one. Exceptions live in `.public-tree-allow`, one per line:
  [content:]GLOB [max=BYTES] [blob=SHA_PREFIX[,SHA_PREFIX]] # reason (the reason is mandatory)
A plain entry exempts the path from the name/extension/source rules; `max=` also lifts the size limit for it
(size is otherwise still enforced); `blob=` pins the entry to those exact blobs, so replacing an allowed file
by something else at the same path is caught; `content:` exempts the content rules (markers, secrets, holders).
GLOB: `*` stays inside a directory, `**` crosses directories, a trailing `/` means everything below.
"""

import argparse
import fnmatch
import json
import os
import re
import shlex
import subprocess
import sys

KIB = 1024
MIB = 1024 * KIB
DEFAULT_MAX_BINARY = 256 * KIB
DEFAULT_MAX_TEXT = 2 * MIB
SCAN_CAP = 1 * MIB          # content rules read at most this much of a blob; bigger blobs hit the size rules
BINARY_PROBE = 8000
HEADER_LINES = 60           # copyright headers are looked for in the first lines of a file
CHUNK_BYTES = 32 * MIB      # blobs are read in batches of at most this many bytes (RAM bound)
ZERO_OID = "0" * 40

DEFAULT_ROOTS = {".local", "decompiled", "decompiled-src", "decomp", "gamedata", "game-data", "gamefiles",
                 "game-files", "extracted", "run", "logs", ".minecraft", "saves", "crash-reports"}
DEFAULT_DIRS = {"decompiled", "decompiled-src", "jadx-out", "vineflower-out", "cfr-out", ".minecraft"}
DEFAULT_EXTS = {
    # War Thunder, Warframe, Hytale, Unity
    ".vromfs.bin", ".grp", ".dxp.bin", ".ddsx", ".blk", ".toc", ".cache",
    ".assets", ".ress", ".resource", ".unity3d", ".assetbundle", ".bundle",
    # Minecraft
    ".jar", ".class", ".mca", ".mcr", ".mcc",
    # profiling and crash dumps of the game process
    ".jfr", ".hprof",
}
DEFAULT_NAMES = {
    "level.dat", "level.dat_old", "level.dat_mcr", "oo2core*.dll", "oo2core*.so*", "oo2core*.dylib", "liboo2core*",
    "oodle*.dll", "liboodle*", "*assets.zip", "hytale*.zip", "globalgamemanagers*", "unity default resources",
    "unity_builtin_extra", "maindata", "resources.assets*",
}
DEFAULT_BINARY_EXTS = {".exe", ".dll", ".dylib", ".so", ".pdb", ".lib", ".obj", ".o", ".pyd", ".elf", ".dex"}
DEFAULT_SOURCE_EXTS = {".java", ".cs", ".smali", ".jad"}
SECRET_FILE_NAMES = {".env", ".env.*", "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "credentials", "credentials.toml",
                     ".netrc", "*.p12", "*.pfx", "*.jks", "*.keystore", "*.key", "secring.gpg"}
SECRET_FILE_OK = {".env.example", ".env.sample", ".env.template"}
LICENSE_NAMES = ("license", "licence", "copying", "notice", "third_party", "third-party", "authors", "copyright")
NOTICE_NAMES = ("notice", "third_party", "third-party", "thirdparty", "notices")
GAME_PACKAGE_RE = re.compile(r"^\s*package\s+(?:net\.minecraft|com\.mojang|net\.minecraftforge|cpw\.mods)\b", re.M)
GAME_PACKAGE_EXTS = (".java", ".kt", ".scala", ".groovy")

MARKERS = [
    re.compile(r"Decompiled (?:with|by) (?:CFR|Fernflower|Procyon|Vineflower|JD-GUI|Jad|JADX|ILSpy|dnSpy|JetBrains)", re.I),
    re.compile(r"Source code recreated from a \.class file by IntelliJ IDEA"),
    re.compile(r"Powered by (?:Fernflower|Vineflower)"),
    re.compile(r"^\s*//\s*\$FF:", re.M),                       # Fernflower synthetic comments
    re.compile(r"/\*\s*(?:loaded from|renamed from):\s", re.I),  # JADX
    re.compile(r"Java compiler version:\s*\d"),                # JD-GUI
    re.compile(r"Decompiled by J[a]d|J[a]d home page"),
    re.compile(r"This file was (?:automatically )?(?:generated|decompiled) by (?:JADX|jadx|ILSpy|dnSpy)"),
]
SECRETS = [
    ("github-token", re.compile(r"\b(?:gh[pousr]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{22,255})\b")),
    ("crates-io-token", re.compile(r"\bcio[A-Za-z0-9]{32}\b")),
    ("crates-io-token", re.compile(
        r"""(?m)\bCARGO_REGISTR(?:Y|IES_[A-Z0-9_]+)_?TOKEN\s*[:=]\s*["']?(?![\$\{<%])[A-Za-z0-9]{24,}""")),
    ("private-key", re.compile(r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----")),
    ("aws-access-key", re.compile(r"\bAKIA[0-9A-Z]{16}\b")),
    ("npm-token", re.compile(r"\bnpm_[A-Za-z0-9]{36}\b")),
    ("slack-token", re.compile(r"\bxox[abprs]-[A-Za-z0-9-]{10,}\b")),
    ("llm-api-key", re.compile(r"\bsk-(?:ant|proj)-[A-Za-z0-9_-]{20,}\b")),
    ("google-api-key", re.compile(r"\bAIza[0-9A-Za-z_-]{35}\b")),
]
YEARS = r"(?:(?:19|20)\d\d(?:\s*[-–—,]\s*(?:19|20)\d\d)*)"
COPY_WORD_RE = re.compile(r"(?i)(?:copyright|spdx-filecopyrighttext)\s*:?\s*(?P<sym>\(c\)|©)?\s*"
                          r"(?P<years>" + YEARS + r")?\s*,?\s*(?:by\s+)?(?P<rest>.*)")
COPY_SYM_RE = re.compile(r"(?i)(?:©|\(c\))\s*(?P<years>" + YEARS + r")\s*,?\s*(?:by\s+)?(?P<rest>.*)")
HOLDER_CUT_RE = re.compile(r"(?i)\s*(?:all rights reserved|licen[sc]ed (?:under|to)|released under|under the |"
                           r"permission is|this (?:file|program|software|code)|\*/|-->|\bSPDX\b).*")


class GateError(Exception):
    """The gate could not do its job; the caller exits 2."""


# --------------------------------------------------------------------------------------------- globs and config

def glob_re(glob):
    if glob.endswith("/"):
        glob += "**"
    out, i = [], 0
    while i < len(glob):
        if glob.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif glob.startswith("**", i):
            out.append(".*")
            i += 2
        elif glob[i] == "*":
            out.append("[^/]*")
            i += 1
        elif glob[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(glob[i]))
            i += 1
    return re.compile("^" + "".join(out) + "$")


class Allow(object):
    def __init__(self, glob, content, max_bytes, blobs, reason, line):
        self.glob, self.content, self.max_bytes, self.blobs, self.reason, self.line = \
            glob, content, max_bytes, blobs, reason, line
        self.rx = glob_re(glob)

    def applies(self, path, blob, content):
        if self.content != content or not self.rx.match(path):
            return False
        return not self.blobs or any(blob.startswith(b) for b in self.blobs)


class Rules(object):
    def __init__(self):
        self.roots = set(DEFAULT_ROOTS)
        self.dirs = set(DEFAULT_DIRS)
        self.exts = set(DEFAULT_EXTS)
        self.names = set(DEFAULT_NAMES)
        self.path_globs = []
        self.binary_exts = set(DEFAULT_BINARY_EXTS)
        self.source_exts = set(DEFAULT_SOURCE_EXTS)
        self.allow_source = []
        self.max_binary = DEFAULT_MAX_BINARY
        self.max_text = DEFAULT_MAX_TEXT
        self.own_holders = ["patodois"]
        self.markers = list(MARKERS)
        self.allows = []

    def load_config(self, path):
        with open(path, encoding="utf-8") as handle:
            for number, raw in enumerate(handle, 1):
                line = raw.split("#", 1)[0].strip()
                if not line:
                    continue
                parts = line.split(None, 1)
                key, value = parts[0], (parts[1].strip() if len(parts) > 1 else "")
                where = "%s:%d" % (path, number)
                if not value:
                    raise GateError("%s: directive %r needs a value" % (where, key))
                if key == "private_root":
                    self.roots.add(value.lower())
                elif key == "private_dir":
                    self.dirs.add(value.lower())
                elif key == "private_ext":
                    self.exts.add(value.lower() if value.startswith(".") else "." + value.lower())
                elif key == "private_name":
                    self.names.add(value.lower())
                elif key == "private_path":
                    self.path_globs.append(glob_re(value.lower()))
                elif key == "source_ext":
                    self.source_exts.add(value.lower() if value.startswith(".") else "." + value.lower())
                elif key == "allow_source":
                    self.allow_source.append(glob_re(value))
                elif key in ("max_binary_bytes", "max_text_bytes"):
                    try:
                        number_value = int(value)
                    except ValueError:
                        raise GateError("%s: %s needs an integer" % (where, key))
                    if key == "max_binary_bytes":
                        self.max_binary = number_value
                    else:
                        self.max_text = number_value
                elif key in ("own_holder", "notice_holder"):
                    self.own_holders.append(value)
                elif key == "marker":
                    try:
                        self.markers.append(re.compile(value))
                    except re.error as error:
                        raise GateError("%s: bad regex: %s" % (where, error))
                else:
                    raise GateError("%s: unknown directive %r" % (where, key))

    def load_allow(self, path):
        with open(path, encoding="utf-8") as handle:
            for number, raw in enumerate(handle, 1):
                body, _, reason = raw.partition("#")
                body, reason = body.strip(), reason.strip()
                if not body:
                    continue
                where = "%s:%d" % (path, number)
                if not reason:
                    raise GateError("%s: an allow entry needs a reason after '#'" % where)
                tokens = body.split()
                glob, content = tokens[0], False
                if glob.startswith("content:"):
                    glob, content = glob[len("content:"):], True
                max_bytes, blobs = None, []
                for token in tokens[1:]:
                    if token.startswith("max="):
                        try:
                            max_bytes = int(token[4:])
                        except ValueError:
                            raise GateError("%s: bad max=" % where)
                    elif token.startswith("blob="):
                        blobs = [b.lower() for b in token[5:].split(",") if b]
                        if any(len(b) < 7 or not re.match(r"^[0-9a-f]+$", b) for b in blobs):
                            raise GateError("%s: blob= takes hex prefixes of at least 7 digits" % where)
                    else:
                        raise GateError("%s: unknown option %r" % (where, token))
                if not glob:
                    raise GateError("%s: empty glob" % where)
                self.allows.append(Allow(glob, content, max_bytes, blobs, reason, number))

    def allow_for(self, path, blob, content):
        return [a for a in self.allows if a.applies(path, blob, content)]


# ------------------------------------------------------------------------------------------------------- git

class Git(object):
    def __init__(self, repo):
        self.repo = repo
        self.env = dict(os.environ, GIT_OPTIONAL_LOCKS="0", LC_ALL="C")

    def run(self, args, stdin=None, check=True):
        # --no-replace-objects: Git pushes the stored objects, not the history that local replace refs present.
        cmd = ["git", "--no-replace-objects", "-C", self.repo] + list(args)
        try:
            done = subprocess.run(cmd, input=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=self.env)
        except OSError as error:
            raise GateError("cannot run git: %s" % error)
        if check and done.returncode != 0:
            raise GateError("git %s failed: %s" % (" ".join(args[:3]), done.stderr.decode("utf-8", "replace").strip()))
        return done

    def probe(self, sha, count):
        """The first `count` bytes of a blob; the rest is never read."""
        cmd = ["git", "--no-replace-objects", "-C", self.repo, "cat-file", "blob", sha]
        proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=self.env)
        try:
            return proc.stdout.read(count)
        finally:
            proc.kill()
            proc.wait()
            proc.stdout.close()

    def has_object(self, sha):
        return self.run(["cat-file", "-e", sha + "^{commit}"], check=False).returncode == 0

    def entries_index(self):
        out = self.run(["ls-files", "-s", "-z"]).stdout
        found = []
        for record in out.split(b"\0"):
            if not record:
                continue
            meta, _, path = record.partition(b"\t")
            mode, sha, _stage = meta.decode("ascii").split()
            found.append((path.decode("utf-8", "surrogateescape"), sha, mode, None))
        return found

    def entries_history(self, revs):
        """(path, blob, mode, commit) for every non-deleting change; commit = (sha, date, subject)."""
        if not revs:
            return []  # an empty push must never fall back to --all
        args = ["log", "--format=%x01%H%x02%as%x02%s", "--raw", "--no-abbrev", "--no-renames", "--diff-filter=d",
                "-m", "-z"] + revs + ["--"]
        out = self.run(args).stdout
        found, commit, tokens, i = [], None, out.split(b"\0"), 0
        while i < len(tokens):
            token = tokens[i].lstrip(b"\n")
            i += 1
            if token.startswith(b"\x01"):
                sha, date, subject = (token[1:].decode("utf-8", "replace").split("\x02", 2) + ["", ""])[:3]
                commit = (sha, date, subject)
            elif token.startswith(b":"):
                fields = token[1:].decode("ascii", "replace").split()
                if len(fields) < 5 or i >= len(tokens):
                    raise GateError("unexpected git log output")
                path = tokens[i].decode("utf-8", "surrogateescape")
                i += 1
                found.append((path, fields[3], fields[1], commit))
        return found

    def sizes(self, shas):
        if not shas:
            return {}
        done = self.run(["cat-file", "--batch-check"], stdin=("\n".join(shas) + "\n").encode("ascii"))
        sizes = {}
        for line in done.stdout.decode("ascii", "replace").splitlines():
            fields = line.split()
            if len(fields) == 3 and fields[1] == "blob":
                sizes[fields[0]] = int(fields[2])
            else:
                raise GateError("cannot read object: %s (shallow or partial clone? fetch the full history)" % line)
        return sizes

    def read_blobs(self, shas, sizes):
        """Yield (sha, bytes), at most CHUNK_BYTES held at a time."""
        batch, total = [], 0
        for sha in shas + [None]:
            if sha is not None and (not batch or total + sizes[sha] <= CHUNK_BYTES):
                batch.append(sha)
                total += sizes[sha]
                continue
            if batch:
                out = self.run(["cat-file", "--batch"], stdin=("\n".join(batch) + "\n").encode("ascii")).stdout
                pos = 0
                for _ in batch:
                    end = out.index(b"\n", pos)
                    name, kind, size = out[pos:end].decode("ascii").split()
                    pos = end + 1
                    yield name, out[pos:pos + int(size)]
                    pos += int(size) + 1
            batch, total = ([sha], sizes[sha]) if sha is not None else ([], 0)


# ------------------------------------------------------------------------------------------------ the rules

def normalize(text):
    return " ".join(re.sub(r"[^a-z0-9]+", " ", text.lower()).split())


def copyright_holders(text):
    found = []
    for line in text.splitlines()[:HEADER_LINES]:
        low = line.lower()
        if "copyright" not in low and "©" not in line and "(c)" not in low:
            continue
        match = COPY_WORD_RE.search(line)
        if match and not (match.group("sym") or match.group("years")):
            match = None
        if not match:
            match = COPY_SYM_RE.search(line)
        if not match:
            continue
        holder = HOLDER_CUT_RE.sub("", match.group("rest"))
        holder = re.sub(r"<[^>]*>|\(\S+@\S+\)|\S+@\S+", "", holder).strip(" \t,.;:*#/-\"'")
        if len(holder) < 2 or holder[0] in "<[{$%" or holder.lower() in ("yyyy", "year", "holder", "name", "owner"):
            continue
        found.append(holder[:80])
    return found


def is_binary(head):
    return b"\0" in head[:BINARY_PROBE]


def lfs_size(data):
    if data.startswith(b"version https://git-lfs.github.com/spec/v1"):
        match = re.search(rb"^size (\d+)$", data[:512], re.M)
        if match:
            return int(match.group(1))
    return None


def path_findings(rules, path):
    """(rule, detail) for the rules that depend on the path alone."""
    parts = path.split("/")
    lower = path.lower()
    base = parts[-1].lower()
    found = []
    if len(parts) > 1 and parts[0].lower() in rules.roots:
        found.append(("private-root", parts[0]))
    for directory in parts[:-1]:
        if directory.lower() in rules.dirs:
            found.append(("private-dir", directory))
            break
    for ext in sorted(rules.exts):
        if base.endswith(ext):
            found.append(("private-extension", ext))
            break
    for pattern in sorted(rules.names):
        if fnmatch.fnmatchcase(base, pattern):
            found.append(("private-name", pattern))
            break
    for rx in rules.path_globs:
        if rx.match(lower):
            found.append(("private-name", "configured path glob"))
            break
    for ext in sorted(rules.binary_exts):
        if base.endswith(ext):
            found.append(("compiled-binary", ext))
            break
    for ext in sorted(rules.source_exts):
        if base.endswith(ext) and not any(rx.match(path) for rx in rules.allow_source):
            found.append(("foreign-source", ext))
            break
    if base not in SECRET_FILE_OK:
        for pattern in sorted(SECRET_FILE_NAMES):
            if fnmatch.fnmatchcase(base, pattern):
                found.append(("secret-file", pattern))
                break
    return found


def content_findings(rules, path, text, holders_ok):
    found = []
    for rx in rules.markers:
        match = rx.search(text)
        if match:
            found.append(("decompiler-marker", match.group(0).strip()[:60]))
            break
    for name, rx in SECRETS:
        match = rx.search(text)
        if match:
            found.append(("secret-content", "%s (%s...)" % (name, match.group(0).strip()[:6])))
    base = path.rsplit("/", 1)[-1].lower()
    if path.lower().endswith(GAME_PACKAGE_EXTS) and GAME_PACKAGE_RE.search(text):
        found.append(("game-package-decl", "declares a package inside the game"))
    if not base.startswith(LICENSE_NAMES) and not base.endswith(".lock"):
        for holder in copyright_holders(text):
            norm = normalize(holder)
            if norm and not any(len(ok) >= 3 and (ok in norm or norm in ok) for ok in holders_ok):
                found.append(("foreign-copyright", holder))
    return found


def holders_allowed(rules, repo):
    """Our own names plus every NOTICE-like file at the top of the working tree (normalized)."""
    ok = [normalize(h) for h in rules.own_holders]
    try:
        names = os.listdir(repo)
    except OSError:
        names = []
    for name in names:
        full = os.path.join(repo, name)
        if name.lower().startswith(NOTICE_NAMES) and os.path.isfile(full) and os.path.getsize(full) <= 4 * MIB:
            with open(full, encoding="utf-8", errors="replace") as handle:
                ok.append(normalize(handle.read()))
    return [o for o in ok if o]


# ------------------------------------------------------------------------------------------- the audit run

class Hit(object):
    def __init__(self, rule, path, detail, blob, size, commit):
        self.rule, self.path, self.detail, self.blob, self.size = rule, path, detail, blob, size
        self.first = commit   # the oldest commit seen, since git log runs newest first
        self.versions = 1
        self.blobs = {blob}


def audit(git, rules, repo, entries):
    """Return (hits, allowed): lists of Hit; allowed ones carry .reason."""
    # one entry per (path, blob, mode); git log runs newest first, so the last one seen is the oldest commit.
    # Submodule links (mode 160000) hold no files.
    unique = {}
    for path, sha, mode, commit in entries:
        if mode != "160000":
            unique[(path, sha, mode)] = commit
    entries = [(k[0], k[1], k[2], c) for k, c in unique.items()]
    shas = sorted({e[1] for e in entries})
    sizes = git.sizes(shas)
    holders_ok = holders_allowed(rules, repo)
    hits, allowed = {}, {}

    def record(rule, path, detail, blob, size, commit, reasons=None):
        table = allowed if reasons is not None else hits
        key = (rule, path, detail)
        if key in table:
            hit = table[key]
            if blob not in hit.blobs:
                hit.blobs.add(blob)
                hit.versions += 1
            hit.first = commit
        else:
            hit = table[key] = Hit(rule, path, detail, blob, size, commit)
        if reasons is not None:
            hit.reason = "; ".join(sorted(set(reasons)))

    def route(rule, path, detail, blob, size, commit, content):
        exempt = [a for a in rules.allow_for(path, blob, content)]
        if exempt:
            record(rule, path, detail, blob, size, commit, [a.reason for a in exempt])
        else:
            record(rule, path, detail, blob, size, commit)

    # 1. content of the blobs small enough to read, once per blob
    content_by_blob, lfs, binary = {}, {}, {}
    small = [s for s in shas if sizes[s] <= SCAN_CAP]
    paths_of = {}
    for path, sha, _mode, _commit in entries:
        paths_of.setdefault(sha, set()).add(path)
    for sha, data in git.read_blobs(small, sizes):
        declared = lfs_size(data)
        if declared is not None:
            lfs[sha] = declared
        binary[sha] = is_binary(data)
        if not binary[sha]:
            content_by_blob[sha] = data.decode("utf-8", "replace")

    # 2. every (path, blob) pair
    for path, sha, mode, commit in entries:
        size = sizes[sha]
        for rule, detail in path_findings(rules, path):
            route(rule, path, detail, sha, size, commit, False)
        if mode == "120000":
            continue  # a symlink's blob is its target path
        real = lfs.get(sha, size)
        if real > min(rules.max_binary, rules.max_text):
            if sha in lfs:
                is_bin = True
            elif sha in binary:
                is_bin = binary[sha]
            else:
                is_bin = is_binary(git.probe(sha, BINARY_PROBE))
            limit = rules.max_binary if is_bin else rules.max_text
            if real > limit:
                kind = "large-binary" if is_bin else "large-text"
                detail = "%d bytes > limit %d%s" % (real, limit, " (LFS pointer)" if sha in lfs else "")
                exempt = rules.allow_for(path, sha, False)
                caps = [a.max_bytes for a in exempt if a.max_bytes is not None]
                if caps and real <= max(caps):
                    record(kind, path, detail, sha, real, commit, [a.reason for a in exempt])
                elif caps:
                    record(kind, path, "%d bytes > allowed %d" % (real, max(caps)), sha, real, commit)
                else:
                    record(kind, path, detail, sha, real, commit)
        text = content_by_blob.get(sha)
        if text is not None:
            for rule, detail in content_findings(rules, path, text, holders_ok):
                route(rule, path, detail, sha, size, commit, True)
    return sorted(hits.values(), key=lambda h: (h.rule, h.path)), sorted(allowed.values(), key=lambda h: (h.rule, h.path))


# ----------------------------------------------------------------------------------------------- the CLI

def describe(hit):
    if hit.first:
        where = "%s %s %s" % (hit.first[0][:9], hit.first[1], hit.first[2][:60])
    else:
        where = "tracked now"
    extra = " (%d versions)" % hit.versions if hit.versions > 1 else ""
    return "  %s  [%s]  %s%s\n      oldest: %s" % (hit.path, hit.detail, "blob " + hit.blob[:9], extra, where)


def report(hits, allowed, show_allowed, as_json, out):
    if as_json:
        def row(h):
            return {"rule": h.rule, "path": h.path, "detail": h.detail, "blob": h.blob, "size": h.size,
                    "versions": h.versions, "oldest_commit": h.first[0] if h.first else None,
                    "reason": getattr(h, "reason", None)}
        json.dump({"hits": [row(h) for h in hits], "allowed": [row(h) for h in allowed]}, out, indent=1)
        out.write("\n")
        return
    if hits:
        out.write("Private material must stay outside Git (game files, decompiled or third-party code, secrets):\n")
        for rule in sorted({h.rule for h in hits}):
            out.write("\n%s\n" % rule)
            for hit in [h for h in hits if h.rule == rule]:
                out.write(describe(hit) + "\n")
        out.write("\n%d finding(s). If one is ours and harmless, list it with a reason in .public-tree-allow.\n" % len(hits))
    if show_allowed and allowed:
        out.write("\nAllowed by .public-tree-allow (%d):\n" % len(allowed))
        for hit in allowed:
            out.write("  %s  [%s %s]  -- %s\n" % (hit.path, hit.rule, hit.detail, hit.reason))
    if not hits:
        out.write("Public tree check passed%s.\n" % (" (%d allowed)" % len(allowed) if allowed else ""))


def pushed(stream, git):
    positives, negatives = [], []
    for line in stream:
        fields = line.split()
        if not fields:
            continue
        if len(fields) != 4:
            raise GateError("unexpected pre-push input: %r" % line)
        _lref, local, _rref, remote = fields
        if local.strip("0"):
            positives.append(local)
        if remote.strip("0") and git.has_object(remote):
            negatives.append("^" + remote)   # already on the remote, so already public
    return positives + negatives if positives else []


def main(argv=None):
    parser = argparse.ArgumentParser(description="Reject game files, decompiled code and secrets from a public repo.")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--history", action="store_true", help="audit all of local history (every ref and HEAD)")
    mode.add_argument("--range", metavar="SPEC", help="audit what `git rev-list SPEC` selects (BASE..HEAD, or a SHA)")
    mode.add_argument("--pre-push", action="store_true", help="audit the ref updates a pre-push hook gets on stdin")
    parser.add_argument("--repo", default=".", help="repository (default: the current directory)")
    parser.add_argument("--config", help="rules file (default: REPO/.public-tree.cfg when it exists)")
    parser.add_argument("--allow", help="allow-list file (default: REPO/.public-tree-allow when it exists)")
    parser.add_argument("--show-allowed", action="store_true", help="also list what the allow-list suppressed")
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    args = parser.parse_args(argv)
    for stream in (sys.stdout, sys.stderr):
        stream.reconfigure(errors="backslashreplace")   # paths are bytes in Git
    try:
        repo = os.path.abspath(args.repo)
        git = Git(repo)
        top = git.run(["rev-parse", "--show-toplevel"]).stdout.decode().strip()
        rules = Rules()
        for explicit, default, loader in ((args.config, ".public-tree.cfg", rules.load_config),
                                          (args.allow, ".public-tree-allow", rules.load_allow)):
            path = explicit or os.path.join(top, default)
            if explicit and not os.path.isfile(path):
                raise GateError("no such file: %s" % path)
            if os.path.isfile(path):
                loader(path)
        if args.history:
            revs = ["--all"] + (["HEAD"] if git.run(["rev-parse", "-q", "--verify", "HEAD"], check=False).returncode == 0
                                else [])
            entries = git.entries_history(revs)
        elif args.range:
            revs = shlex.split(args.range)
            if not revs or any(r.startswith("-") for r in revs):
                raise GateError("--range takes revisions only")
            entries = git.entries_history(revs)
        elif args.pre_push:
            entries = git.entries_history(pushed(sys.stdin, git))
        else:
            entries = git.entries_index()
        hits, allowed = audit(git, rules, top, entries)
    except GateError as error:
        sys.stderr.write("check_public_tree: %s\n" % error)
        return 2
    report(hits, allowed, args.show_allowed, args.json, sys.stderr if hits and not args.json else sys.stdout)
    return 1 if hits else 0


if __name__ == "__main__":
    sys.exit(main())
