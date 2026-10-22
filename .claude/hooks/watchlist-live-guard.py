#!/usr/bin/env python3
"""PreToolUse guard: an agent may not turn on one of the owner's LIVE switches.

Two switches, one rule. `plxnative-watchlist-live` (below) and `plxnative-livetv-live`: the second
lets a `devtriggers` build send Live TV / DVR writes - tune a channel, create, edit, re-prioritise or
delete a recording rule, stop a recording - to a REAL Plex Media Server instead of the synthetic mock
(`rust-modules/plex/src/plex/live_tv.rs`, `guard::live_armed`). Each has its own owner prefix:
`PLX_WATCHLIST_LIVE_OWNER=1` and `PLX_LIVETV_LIVE_OWNER=1`.

`plxnative-watchlist-live` in a debug install's runtime root (on the television
`/tmp/com.beb.plxnative.debug/`) lets a `devtriggers` build send Watchlist add/remove to the REAL
plex.tv account (`rust-modules/plex/src/plex/watchlist.rs`, `live_armed`). Without it a debug build
refuses every write (the Q11 failsafe), which is what keeps an agent's TV run from changing a
household's Watchlist. The owner creates the file by hand, from their own terminal, when they want to
check real writes; AGENTS.md: no agent creates it unless the owner asks for it in that same session.

What is refused is a Bash command that would CREATE the file: `touch`, `tee`, `cp`/`mv`/`install`/
`ln`/`dd`/`rsync`/`scp`, a `>`/`>>` redirection onto it, a script that opens it for writing, or
`tools/tv-session.sh up --arm watchlist-live`, also when the command is wrapped in
`tools/tv-ssh ssh tv "..."` or `ssh root@... "..."` (the inner string is part of the same command
line). Reading, grepping, listing, testing for and REMOVING it stay allowed, and so does an Edit or
Write of source or docs that merely name it (this hook sees Bash only).

THE ESCAPE HATCH is a prefix on the command itself — `PLX_WATCHLIST_LIVE_OWNER=1 …` — for the
owner, or for an agent the owner told IN THAT SESSION to turn the switch on. An agent reaching for it
on its own is deciding, on the household's behalf, to let a test build write to their real account.

FAIL-OPEN ON ITS OWN BUGS, fail-closed on the answer, like `outbound-guard.py`: exit 0 allows, exit 2
blocks and feeds stderr back to the model.
"""
import json
import re
import sys

# The switches: bare name -> the owner's prefix that lets that one through.
SWITCHES = {"watchlist-live": "PLX_WATCHLIST_LIVE_OWNER", "livetv-live": "PLX_LIVETV_LIVE_OWNER"}
# Commands that write a file named among their arguments.
WRITERS = re.compile(r"(?:^|[\s;&|('\"`])(?:touch|tee|cp|mv|install|ln|dd|rsync|scp|truncate)(?=\s)")
# Statement separators. A pipe stays inside the segment on purpose: `echo 1 | tee <path>`.
SPLIT = re.compile(r"&&|\|\||;|\n")


def creates_switch(cmd, switch):
    """Does `cmd` create `plxnative-<switch>`? Returns the offending segment, or None."""
    if re.search(r"\b%s=1\b" % SWITCHES[switch], cmd):
        return None
    sw = re.escape(switch)
    name = re.compile(r"plxnative-%s\b" % sw)
    # `--arm <switch>` / `--arm=<switch>[=…]` on tv-session.sh writes plxnative-<switch>.
    arm = re.compile(r"--arm(?:=|\s+)['\"]?%s\b" % sw)
    # A redirection whose target is the trigger: `> …plxnative-<switch>`, `>>`, `>|`, `&>`.
    redirect = re.compile(r"(?:\d?>>?|>\||&>)\s*['\"]?[^\s;&|'\"]*plxnative-%s\b" % sw)
    # A script that opens it for writing: python `open(…, 'w'|'a'|'x')`, `.write_text(`, `.touch(`.
    scripted = re.compile(r"open\([^)]*plxnative-%s[^)]*,\s*['\"][wax]|"
                          r"plxnative-%s[^;\n]*\.(?:write_text|write_bytes|touch)\(" % (sw, sw))
    if arm.search(cmd):
        return arm.search(cmd).group(0)
    if not name.search(cmd):
        return None
    if redirect.search(cmd) or scripted.search(cmd):
        return cmd
    for seg in SPLIT.split(cmd):
        if name.search(seg) and WRITERS.search(seg):
            return seg.strip()
    return None


def creates(cmd):
    """Does `cmd` create any live switch? Returns the offending segment, or None."""
    for switch in SWITCHES:
        seg = creates_switch(cmd, switch)
        if seg is not None:
            return seg
    return None


def which(cmd):
    """The first switch `cmd` would create, or None."""
    return next((s for s in SWITCHES if creates_switch(cmd, s) is not None), None)


def main():
    raw = sys.stdin.read()
    try:
        payload = json.loads(raw)
    except Exception:
        return 0
    if not isinstance(payload, dict) or payload.get("tool_name") != "Bash":
        return 0
    ti = payload.get("tool_input")
    cmd = ti.get("command") if isinstance(ti, dict) else None
    if not isinstance(cmd, str) or not cmd.strip():
        return 0
    seg = creates(cmd)
    if seg is None:
        return 0
    if which(cmd) == "livetv-live":
        sys.stderr.write(
            "BLOCKED: this command would turn on the owner's live Live TV / DVR switch "
            "(plxnative-livetv-live).\n"
            "  the command: %s\n\n"
            "With that file present a debug build tunes channels and creates, edits and deletes DVR\n"
            "recordings on the household's REAL Plex Media Server. AGENTS.md: no agent creates it unless\n"
            "the owner asks for it in this same session. Test against the mock instead\n"
            "(`tests/mock_pms.py --livetv`). If the owner did ask, prefix the command with\n"
            "PLX_LIVETV_LIVE_OWNER=1.\n" % seg[:160])
        return 2
    sys.stderr.write(
        "BLOCKED: this command would turn on the owner's live Watchlist switch "
        "(plxnative-watchlist-live).\n"
        "  the command: %s\n\n"
        "With that file present a debug build writes Watchlist add/remove to the REAL plex.tv account.\n"
        "AGENTS.md: no agent creates it unless the owner asks for it in this same session. Test writes\n"
        "against the mock instead (`tools/tv-session.sh up --mock-plextv=PORT` arms the discover\n"
        "stand-in). If the owner did ask, prefix the command with PLX_WATCHLIST_LIVE_OWNER=1.\n"
        % seg[:160])
    return 2


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:      # a bug in the guard must not wedge every Bash call
        sys.stderr.write("watchlist-live-guard: internal error, allowing (%s)\n" % e)
        sys.exit(0)
