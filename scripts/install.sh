#!/bin/sh
# Axocoatl installer — downloads a prebuilt binary from GitHub Releases.
# Usage: curl -fsSL https://axocoatl.ai/install.sh | sh
#
# It installs to /usr/local/bin when that directory is writable, else to
# ~/.local/bin. When the directory is not on PATH it asks, on the terminal,
# before adding one line to the startup files your shell reads in a new
# terminal and a new login (~/.zprofile for zsh on macOS; ~/.zshrc and
# ~/.zprofile for zsh on Linux; for bash the login file that exists, else
# ~/.bash_profile on macOS and ~/.profile on Linux, and on Linux ~/.bashrc
# too; fish's config.fish; ~/.profile otherwise). The line adds the directory
# only when PATH does not have it. A file gets none when it has this
# installer's marked line or its guarded case line anywhere, or, where the
# installer can tell nothing before them keeps them from running, the export
# line 1.4.0 printed, fish's line, or the lines Debian's and Ubuntu's
# ~/.profile use to add ~/.local/bin (adds_to_path, which also says what the
# installer cannot see, such as set -e and a failing command, a sourced file
# that exits, or a syntax error); next to such a ~/.profile the ~/.bashrc
# line skips login shells.
# Without a terminal (or when the answer is end of input) it only prints that
# line. In a GitHub Actions step it adds the directory to $GITHUB_PATH
# instead, for the job's later steps.
# AXOCOATL_MODIFY_PATH=yes adds the line without asking;
# AXOCOATL_MODIFY_PATH=no never changes a profile or $GITHUB_PATH.
# AXOCOATL_VERSION=v1.5.0 installs that release instead of the latest one, and
# AXOCOATL_INSTALL_BASE_URL (https:// or file://, with AXOCOATL_VERSION) names
# the directory that holds its archive and .sha256 file instead of the GitHub
# release; the checksum is still required.
# AXOCOATL_INSTALL_SYSTEM_DIR and AXOCOATL_INSTALL_TTY replace /usr/local/bin
# and /dev/tty for scripts/test-install.sh, and AXOCOATL_INSTALL_ROOT stands
# in for / where it looks for a podman that is not on PATH.
set -eu

REPO="axocoatl/axocoatl"
BIN="axocoatl"

err() { echo "axocoatl-install: $*" >&2; exit 1; }
info() { echo "axocoatl-install: $*"; }

modify_path="${AXOCOATL_MODIFY_PATH:-ask}"
case "$modify_path" in
  ask|yes|no) ;;
  *) err "AXOCOATL_MODIFY_PATH must be ask, yes or no, not '$modify_path'" ;;
esac
# A release tag such as v1.5.0 (or 1.5.0), checked before any download.
requested_tag="${AXOCOATL_VERSION:-}"
case "$requested_tag" in
  '') ;;
  *[!A-Za-z0-9.-]*) err "AXOCOATL_VERSION must be a release tag such as v1.5.0, not '$requested_tag'" ;;
  v[0-9]*) ;;
  [0-9]*) requested_tag="v$requested_tag" ;;
  *) err "AXOCOATL_VERSION must be a release tag such as v1.5.0, not '$requested_tag'" ;;
esac
base_url="${AXOCOATL_INSTALL_BASE_URL:-}"
case "$base_url" in
  '') ;;
  https://*|file://*)
    [ -n "$requested_tag" ] \
      || err "AXOCOATL_INSTALL_BASE_URL needs AXOCOATL_VERSION, which names the release to install from it"
    base_url="${base_url%/}"
    ;;
  *) err "AXOCOATL_INSTALL_BASE_URL must start with https:// or file://, not '$base_url'" ;;
esac
system_dir="${AXOCOATL_INSTALL_SYSTEM_DIR:-/usr/local/bin}"
tty_device="${AXOCOATL_INSTALL_TTY:-/dev/tty}"
root_dir="${AXOCOATL_INSTALL_ROOT:-}"

# A path as the user knows it: ~/... under the home directory.
display_path() {
  case "$1" in
    "$HOME"/*) printf '~/%s' "${1#"$HOME"/}" ;;
    *) printf '%s' "$1" ;;
  esac
}

# A path quoted for a shell command line when it needs quoting.
shell_word() {
  case "$1" in
    *[!A-Za-z0-9/._+:-]*) printf "'%s'" "$1" ;;
    *) printf '%s' "$1" ;;
  esac
}

# The lines that put directory expression $1 ($HOME/... or a full path) on
# PATH: first on PATH for this shell; the same only when PATH does not have it
# yet, for startup files; and fish's, which changes only PATH (without --path,
# fish_add_path also saves the directory in the universal fish_user_paths,
# which outlives the line).
# shellcheck disable=SC2016
export_line() { printf 'export PATH="%s:$PATH"' "$1"; }
# shellcheck disable=SC2016
guarded_line() { printf 'case ":$PATH:" in *":%s:"*) ;; *) export PATH="%s:$PATH" ;; esac' "$1" "$1"; }
fish_line() { printf 'fish_add_path --path "%s"' "$1"; }
# Before the ~/.bashrc line next to a ~/.profile that adds the directory.
nonlogin_prefix='shopt -q login_shell || '

# The installer's own lines for directory expression $1, one per line: for
# sh, bash and zsh files the guarded line, the same after the ~/.bashrc
# prefix, and the unguarded export line, which installers up to 1.4.0 printed
# for the user to add (with the full path) and unreleased 1.5.0 builds wrote
# (with $HOME); for fish's config.fish the fish line, the one without --path
# that unreleased 1.5.0 builds wrote, and the export line, which fish runs
# too.
own_sh_lines() {
  guarded_line "$1"
  echo
  printf '%s' "$nonlogin_prefix"
  guarded_line "$1"
  echo
  export_line "$1"
}
own_fish_lines() {
  fish_line "$1"
  echo
  printf 'fish_add_path "%s"\n' "$1"
  export_line "$1"
}

# Awk functions for reading an sh, bash or zsh startup file, for the checks
# below. Call sure() before a line and track(line) after it. sure() is true
# only when all of these hold for that line:
#   - no block is open: if, case, for, while, until, select, { } or ( ),
#     which covers function bodies too;
#   - no quote, $( ), ${ }, backquote or ( ) inside a word (a=( ... )) is
#     open;
#   - the line before did not end in &&, ||, |, |&, \, !, time or a function
#     header still waiting for its body, and no [[ ]] is open;
#   - no return, exit, exec, logout or bye came before it, anywhere, also
#     written with a backslash or quotes ('return', "ex"it) (ended), except
#     an exec where a command starts with only redirections after it
#     (exec 2>>log), which changes this shell's files and goes on;
#   - the reader did not lose track (lost) before it, at a here-document, a
#     closing word that does not match the open block (fi in a { }, } in an
#     if), a redirection without a target, an operator where a command
#     should start (a line that starts with |), a case inside $( ), <( ) or
#     >( ), or a construct it does not follow (( where no command starts, as
#     in for ((...)) and zsh's for f (...); zsh's foreach and repeat;
#     coproc).
# It splits words much as the shell does, with quotes, backslashes and
# comments (also in $( ), <( ), >( ) and in a ( ) word), and takes a keyword
# only where a command starts; after builtin, command (also command -p or
# command --) or an assignment (FOO=1) a command still starts, and digits or a
# {NAME} right before < or > are the descriptor a redirection names
# (2>/dev/null, {fd}>file), not a word. Where it sees that it cannot follow
# the file, later lines do not count: it opens a block for any word that ends
# in {, but closes one only at a } where a command starts, so after zsh's
# `f() { cmd }` no later line counts. So sure() means only that the installer
# can tell nothing before the line keeps it from running. What it cannot see:
# a command that stops the shell another way (set -e and a failing command, a
# sourced file or an eval that returns or exits), a syntax error it does not
# notice, after which the shell stops reading the file, and a later line that
# takes the directory out of PATH again; a valid file it misreads in some
# other way can make a line count too. It also numbers each block it opens
# (bid), sets branched after an else or elif of a block at the top level, and
# sets cmtat to where a comment starts on the line it read (0 for none).
# It sets stopped at a return, exit, exec, logout or bye where a command
# starts (an exec only when a command follows it), unless that word is in a
# function's body or a ( ) subshell, which it ends instead of the shell, or
# stops only a shell that is not interactive: Ubuntu's and Debian's
# `case $- in *i*) ;; *) return;; esac` (a branch after a *i* branch of a
# case on $-), Arch's `[[ $- != *i* ]] && return`, `[ -z "$PS1" ] &&
# return`, zsh's `[[ -o interactive ]] || return`, the same tests the other
# way round with the other operator (each also as return N), and Gentoo's
# `if [[ $- != *i* ]]; then return; fi` (the then branch of an if on such a
# test, on one line or several). Every new terminal is an interactive shell,
# so those do not keep one from reading a later line.
# swap(s, from, to) replaces every from in s with to, trim(s) removes leading
# and trailing blanks, and unq(s) is word s without its quotes and
# backslashes.
# shellcheck disable=SC2016
startup_awk='
BEGIN {
  sq = sprintf("%c", 39)
  nctx = 0; nblk = 0; nbid = 0; cmd = 1; cont = 0; lost = 0; ended = 0; branched = 0
  testmode = 0; pending = 0; redir = 0; fname = 0; fnparen = 0; afterfname = 0
  prevcmd = 0; word = ""; raw = ""; inword = 0; last = ""; ctxbs = 0; stopped = 0
  cmtat = 0; curline = ""; linewords = 0; execpend = 0; exechere = 0
  assign = 0; cmdopts = 0
  # Tests true only in a shell that is not interactive (ni), and only in one
  # that is (it), for the stops that stopped leaves out.
  ps1 = "\"?\\$(PS1|\\{PS1\\})\"?"
  dash = "\"?\\$-\"?"
  ni = "(\\[\\[[ \t]+" dash "[ \t]+!=[ \t]+\\*i\\*[ \t]+\\]\\]|\\[[ \t]+-z[ \t]+" ps1 "[ \t]+\\]|\\[\\[[ \t]+-z[ \t]+" ps1 "[ \t]+\\]\\]|\\[\\[[ \t]+![ \t]+-o[ \t]+interactive[ \t]+\\]\\])"
  it = "(\\[\\[[ \t]+" dash "[ \t]+==?[ \t]+\\*i\\*[ \t]+\\]\\]|\\[[ \t]+-n[ \t]+" ps1 "[ \t]+\\]|\\[\\[[ \t]+-n[ \t]+" ps1 "[ \t]+\\]\\]|\\[\\[[ \t]+-o[ \t]+interactive[ \t]+\\]\\])"
  ni_line = "^[ \t]*(" ni "[ \t]*&&|" it "[ \t]*\\|\\|)[ \t]*return([ \t]+[0-9]+)?[ \t]*;?[ \t]*(#.*)?$"
  ni_if = "^[ \t]*if[ \t]+" ni "[ \t]*(;[ \t]*then([ \t].*)?|(#.*)?)$"
}
function swap(s, from, to,    at, out) {
  if (from == "") return s
  out = ""
  while ((at = index(s, from)) > 0) {
    out = out substr(s, 1, at - 1) to
    s = substr(s, at + length(from))
  }
  return out s
}
function trim(s) {
  sub(/^[ \t]+/, "", s)
  sub(/[ \t]+$/, "", s)
  return s
}
function unq(s) {
  gsub("\\$[\"" sq "]", "", s)
  gsub("[\\\\\"" sq "]", "", s)
  return s
}
function sure() {
  return !lost && !ended && !nctx && !nblk && !cont && !pending && !testmode && !fname && !fnparen && !afterfname
}
# Whether a stop where a command starts is in a function body (fnb) or a
# ( ) subshell, or stops only a shell that is not interactive: in a case on
# $- after its *i* branch (icase, iseen), in the then branch of an if on an
# ni test (niif), or on an ni_line.
function harmless(    j) {
  if (pending) return 1
  for (j = 1; j <= nblk; j++) {
    if (fnb[j] || blk[j] == "(" || (blk[j] == "case" && icase[j] && iseen[j]) || (blk[j] == "if" && niif[j])) return 1
  }
  return curline ~ ni_line
}
function push(kind, m) {
  nblk++; blk[nblk] = kind; mode[nblk] = m; bid[nblk] = ++nbid
  fnb[nblk] = 0; icase[nblk] = 0; ipat[nblk] = 0; iseen[nblk] = 0; niif[nblk] = 0
}
function shut(kind, m) {
  if (nblk && blk[nblk] == kind && (m == "" || mode[nblk] == m)) nblk--
  else lost = 1
}
function tok_word(w,    k, m, opens, u, wasopts) {
  linewords++
  prevcmd = 0
  afterfname = 0
  last = w
  if (fnparen) { lost = 1; return }
  if (redir) { redir = 0; return }
  assign = 0
  wasopts = cmdopts; cmdopts = 0
  # A word after an exec where a command starts, other than the targets of
  # its redirections: exec runs that command in place of the shell.
  if (execpend) {
    execpend = 0
    ended = 1
    if (exechere) stopped = 1
  }
  # A word that stops the shell, also with a backslash or quotes in it
  # (\return, "return"), and bye in zsh; anywhere, not only where a command
  # starts. Where a command starts, it stops the file (stopped) unless it is
  # harmless; an exec there only once a command follows it (above).
  u = unq(raw)
  if (u == "return" || u == "exit" || u == "exec" || u == "logout" || u == "bye") {
    if (u == "exec" && cmd && !testmode) { execpend = 1; exechere = !harmless() }
    else {
      ended = 1
      if (cmd && !testmode && !harmless()) stopped = 1
    }
  }
  if (testmode) { if (w == "]]") { testmode = 0; cmd = 0 } return }
  k = nblk ? blk[nblk] : ""
  m = nblk ? mode[nblk] : ""
  if (k == "case" && m == "head") { if (w == "in") mode[nblk] = "pat"; else if (u == "$-") icase[nblk] = 1; return }
  if (k == "case" && m == "pat") { if (w == "esac") { nblk--; cmd = 0 } else if (u == "*i*") ipat[nblk] = 1; return }
  if (k == "loop" && m == "head") { if (w == "do") { mode[nblk] = "body"; cmd = 1 } return }
  opens = (substr(w, length(w)) == "{")
  if (fname) {
    fname = 0
    cmd = 1
    if (opens) { push("{", ""); fnb[nblk] = 1; return }
    pending = 1
    if (w !~ /\)$/) afterfname = 1
    return
  }
  if (opens) { push("{", ""); fnb[nblk] = pending; cmd = 1; pending = 0; return }
  if (!cmd) return
  pending = 0
  # After an assignment (FOO=1 exec zsh), and after command and its -p or
  # --, a command still starts.
  if (w ~ /^[A-Za-z_][A-Za-z0-9_]*\+?=/) { assign = 1; return }
  if (w == "command" || (wasopts && (w == "-p" || w == "--"))) { cmdopts = 1; return }
  if (w == "if") { push("if", ""); niif[nblk] = (linewords == 1 && curline ~ ni_if); return }
  if (w == "then") { if (k != "if") lost = 1; return }
  if (w == "elif" || w == "else") {
    if (k != "if") lost = 1
    else { niif[nblk] = 0; if (nblk == 1) branched = 1 }
    return
  }
  if (w == "fi") { shut("if", ""); cmd = 0; return }
  if (w == "case") { push("case", "head"); cmd = 0; return }
  if (w == "for" || w == "select") { push("loop", "head"); cmd = 0; return }
  if (w == "while" || w == "until") { push("loop", "cond"); return }
  if (w == "do") { if (k == "loop" && m == "cond") mode[nblk] = "body"; else lost = 1; return }
  if (w == "done") { shut("loop", "body"); cmd = 0; return }
  if (w == "esac") { shut("case", "cmd"); cmd = 0; return }
  if (w == "}") { shut("{", ""); cmd = 0; return }
  if (w == "[[") { testmode = 1; cmd = 0; return }
  if (w == "function") { fname = 1; cmd = 0; return }
  if (w == "!" || w == "time" || w == "noglob" || w == "nocorrect" || w == "builtin") return
  if (w == "foreach" || w == "repeat" || w == "coproc" || w == "end") { lost = 1; return }
  if (w ~ /\(\)$/ && w !~ /=/) { pending = 1; return }
  cmd = 0
  prevcmd = 1
}
function tok_op(o,    k, m, wasprev, wasafter, wasassign) {
  wasprev = prevcmd; prevcmd = 0
  wasafter = afterfname; afterfname = 0
  wasassign = assign; assign = 0
  cmdopts = 0
  last = o
  if (redir) { redir = 0; lost = 1 }
  if (testmode) return
  if (o == "<<" || o == "<<-") { lost = 1; return }
  if (o == "<" || o == ">" || o == ">>" || o == "<>" || o == ">|" || o == "&>" || o == "&>>" || o == ">&" || o == "<&" || o == "<<<") { redir = 1; assign = wasassign; return }
  execpend = 0
  # An operator after assignments alone (FOO=1; cmd) ends that command.
  if (wasassign) cmd = 0
  if (o == ")" && fnparen) { fnparen = 0; pending = 1; cmd = 1; return }
  if (fnparen) { lost = 1; return }
  if (o == "(" && (wasprev || wasafter)) { fnparen = 1; return }
  k = nblk ? blk[nblk] : ""
  m = nblk ? mode[nblk] : ""
  if (k == "case" && m == "pat") {
    if (o == ")") { mode[nblk] = "cmd"; cmd = 1 }
    else if (o != "(" && o != "|") lost = 1
    return
  }
  if (k == "case" && m == "head") { lost = 1; return }
  if (k == "loop" && m == "head") { if (o != ";") lost = 1; return }
  if (o == ";;" || o == ";&" || o == ";;&" || o == ";|") {
    if (k == "case" && m == "cmd") {
      mode[nblk] = "pat"
      cmd = 0
      # Only ;; ends the match: after ;& the next branch runs too, and after
      # ;;& or ;| the next patterns are tested.
      if (o == ";;" && ipat[nblk]) iseen[nblk] = 1
      if (o != ";&") ipat[nblk] = 0
    } else lost = 1
    return
  }
  if (o == "(") { if (cmd) { push("(", ""); fnb[nblk] = pending; pending = 0 } else lost = 1; return }
  if (o == ")") { shut("(", ""); cmd = 0; return }
  if (cmd) lost = 1
  cmd = 1
}
function newline(    k, m) {
  execpend = 0
  assign = 0
  cmdopts = 0
  if (redir || fnparen || fname) lost = 1
  redir = 0
  fnparen = 0
  fname = 0
  cont = 0
  if (testmode) { cont = 1; return }
  if (last == "&&" || last == "||" || last == "|" || last == "|&" || last == "!" || last == "time") { cont = 1; return }
  if (pending || afterfname) cont = 1
  k = nblk ? blk[nblk] : ""
  m = nblk ? mode[nblk] : ""
  last = ""
  if (k == "case" && m != "cmd") return
  if (k == "loop" && m == "head") return
  cmd = 1
}
function open_ctx(t, n) { nctx++; ctx[nctx] = t; par[nctx] = n }
function shut_ctx(c) { nctx--; if (!nctx) word = word c }
# The word that ends at i on line s: raw gets its text, quotes and all.
function endword(s, i) {
  raw = raw substr(s, wstart, i - wstart)
  tok_word(word)
  word = ""; raw = ""; inword = 0
}
function track(s,    i, n, c, t, two, three, op, bs, esc, before, escaped) {
  n = length(s)
  bs = 0
  curline = s
  linewords = 0
  cmtat = 0
  # wstart: where the word being read starts on this line (1 for one that
  # goes on from the line before, whose text so far is in raw).
  wstart = 1
  # esc: where the last character written with a backslash is on this line
  # (0 for the line break before it, when the line before ended in one).
  esc = ctxbs ? 0 : -1
  ctxbs = 0
  for (i = 1; i <= n; i++) {
    c = substr(s, i, 1)
    if (nctx) {
      t = ctx[nctx]
      if (t == "S") { if (c == sq) shut_ctx(c); continue }
      if (c == "\\") { if (i == n) ctxbs = 1; esc = i + 1; i++; continue }
      if (t == "E") { if (c == sq) shut_ctx(c); continue }
      if (t == "B") { if (c == "`") shut_ctx(c); continue }
      if (t == "D" || t == "V") {
        if (t == "D" && c == "\"") { shut_ctx(c); continue }
        if (t == "V" && c == "}") { shut_ctx(c); continue }
        if (t == "V" && c == "\"") { open_ctx("D", 0); continue }
        if (c == "`") { open_ctx("B", 0); continue }
        if (c == "$" && substr(s, i + 1, 1) == "{") { open_ctx("V", 0); i++; continue }
        if (c == "$" && substr(s, i + 1, 2) == "((") { open_ctx("R", 2); i += 2; continue }
        if (c == "$" && substr(s, i + 1, 1) == "(") { open_ctx("C", 1); i++; continue }
        continue
      }
      # C ($( ), <( ), >( )), R ($(( ))) and P (( ) in a word) end at their
      # closing ).
      if (c == sq && t != "R") { open_ctx("S", 0); continue }
      if (c == "\"") { open_ctx("D", 0); continue }
      if (c == "`") { open_ctx("B", 0); continue }
      if (c == "$" && substr(s, i + 1, 1) == "{") { open_ctx("V", 0); i++; continue }
      if (c == "$" && substr(s, i + 1, 2) == "((") { open_ctx("R", 2); i += 2; continue }
      if (c == "$" && substr(s, i + 1, 1) == "(") { open_ctx("C", 1); i++; continue }
      if (c == "(") { par[nctx]++; continue }
      if (c == ")") {
        if (--par[nctx] == 0) shut_ctx(c)
        continue
      }
      if (t == "C" && substr(s, i, 3) == "<<<") { i += 2; continue }
      if (t == "C" && substr(s, i, 2) == "<<") lost = 1
      # A case in $( ) (or <( ), >( )) has patterns that end in ), which this
      # count of parentheses would take as the end of the $( ).
      if (t == "C" && substr(s, i, 4) == "case" && (i == 1 || substr(s, i - 1, 1) ~ /[ \t;(|&]/) && (i + 4 > n || substr(s, i + 4, 1) ~ /[ \t;]/)) lost = 1
      # A comment: a # at the start of a line, or after a blank (in $( )
      # also after ; or (), unless that line break or character is written
      # with a backslash (x\ #y, x\;#y, and x\ with #y on the next line, are
      # each one word). In a ( ) word (an array over several lines), not
      # (#...), which is a zsh glob flag there. In $( ), a # after a blank
      # written with a backslash starts a comment too, as bash 3.2 reads it:
      # the reader then stays in the $( ) longer, where no line counts.
      if (c == "#" && (t == "C" || t == "P")) {
        before = (i == 1) ? "" : substr(s, i - 1, 1)
        escaped = (i == 1) ? (esc == 0) : (esc == i - 1)
        if (t == "P" && !escaped && (i == 1 || before ~ /[ \t]/)) i = n
        if (t == "C" && (before ~ /[ \t]/ || (!escaped && (i == 1 || before ~ /[;(]/)))) i = n
      }
      continue
    }
    if (!inword) wstart = i
    if (c == "\\") {
      if (i == n) { bs = 1; break }
      word = word substr(s, i, 2); inword = 1; i++
      continue
    }
    if (c == "#" && !inword) { cmtat = i; break }
    if (c == " " || c == "\t") {
      if (inword) endword(s, i)
      continue
    }
    if (c == sq) { open_ctx("S", 0); word = word c; inword = 1; continue }
    if (c == "\"") { open_ctx("D", 0); word = word c; inword = 1; continue }
    if (c == "`") { open_ctx("B", 0); word = word c; inword = 1; continue }
    if (c == "$") {
      two = substr(s, i + 1, 1)
      if (two == sq) { open_ctx("E", 0); word = word "$" sq; inword = 1; i++; continue }
      if (two == "{") { open_ctx("V", 0); word = word "${"; inword = 1; i++; continue }
      if (substr(s, i + 1, 2) == "((") { open_ctx("R", 2); word = word "$(("; inword = 1; i += 2; continue }
      if (two == "(") { open_ctx("C", 1); word = word "$("; inword = 1; i++; continue }
      word = word c; inword = 1
      continue
    }
    if (c == "(" && inword) { open_ctx("P", 1); word = word c; continue }
    # <( ) and >( ) hold commands, as $( ) does.
    if ((c == "<" || c == ">") && substr(s, i + 1, 1) == "(") {
      open_ctx("C", 1); word = word c "("; inword = 1; i++
      continue
    }
    if (index(";&|<>()", c)) {
      # Digits or a {NAME} right before < or > name the descriptor it
      # redirects.
      if (inword && (c == "<" || c == ">") && word ~ /^([0-9]+|\{[A-Za-z_][A-Za-z0-9_]*\})$/ && raw == "") { word = ""; inword = 0 }
      else if (inword) endword(s, i)
      three = substr(s, i, 3)
      two = substr(s, i, 2)
      if (three == ";;&" || three == "<<<" || three == "<<-" || three == "&>>") op = three
      else if (two == ";;" || two == ";&" || two == ";|" || two == "&&" || two == "||" || two == "|&" || two == ">>" || two == "<<" || two == "<>" || two == ">|" || two == "&>" || two == ">&" || two == "<&") op = two
      else op = c
      i += length(op) - 1
      tok_op(op)
      continue
    }
    word = word c; inword = 1
  }
  # A word in a quote or $( ) goes on on the next line, after a line break
  # (none after a backslash there, which joins the lines).
  if (nctx) {
    raw = raw (ctxbs ? substr(s, wstart, n - wstart) : substr(s, wstart) "\n")
    return
  }
  # A line that ends in a backslash goes on on the next line, and so does
  # the word it ends in (a # that starts the next line is part of it).
  if (bs) { raw = raw substr(s, wstart, n - wstart); cont = 1; return }
  if (inword) endword(s, n + 1)
  newline()
}
'

# Whether startup file $1 counts as putting the install directory ($dest) on
# PATH, so that it gets no line. The directory may be written $HOME/...,
# ${HOME}/... (not in fish) or as its full path. Lines are compared without
# leading and trailing blanks. It counts when any of these holds:
#   - a line right after the installer's marker comment (# Added by the
#     Axocoatl installer) is one of the installer's own lines for its shell
#     (own_sh_lines for a file that does not end in .fish, own_fish_lines for
#     one that does), anywhere in the file, at any depth: the installer
#     writes the marker and its line together, so no construct before them
#     can make a reinstall add them again, and a user who moved them into a
#     block (an OS check, a function) is taken at their word;
#   - in a file that does not end in .fish, a line is the guarded line (or
#     the same after the ~/.bashrc prefix), anywhere: nothing but this
#     installer writes that case statement;
#   - an own line without the marker (the export line installers up to 1.4.0
#     printed for the user to add, or a fish line) is on a line where the
#     installer can tell nothing before it keeps it from running: sure() for
#     sh, bash and zsh; for fish, outside any block (if, for, while,
#     function, switch, begin ... end), before any return, exit or exec
#     (where a command starts: also after &&, ||, and, or, not), before any
#     line fish_track does not follow, and not on a line that goes on from
#     one ending in \, &&, || or |;
#   - in a file that does not end in .fish, it has the three lines Debian's
#     and Ubuntu's ~/.profile use, starting on a line sure() is true for:
#       if [ -d "$HOME/.local/bin" ] ; then
#           PATH="$HOME/.local/bin:$PATH"
#       fi
# Nothing else counts: not a subdirectory, a line that takes the directory
# out of PATH or names it in a comment, an own line with more on its line,
# nor an unmarked own line or Debian's lines in a block, after a line that
# ends in && or \, or after a return. Neither reader can see what the
# startup_awk comment lists (set -e and a failing command, a sourced file
# that exits, a syntax error, a later line that takes the directory out of
# PATH again), so in such a file a line can count that does not run. The
# installer's line does nothing when PATH has the directory, so one line too
# many costs at most a second listing of it (where a later line adds it
# whether or not PATH has it, as Debian's lines do), while counting a file
# wrongly would leave new terminals without axocoatl.
adds_to_path() {
  [ -f "$1" ] || return 1
  case "$dest" in
    "$HOME"/*) dest_rel="${dest#"$HOME"/}" ;;
    *) dest_rel="" ;;
  esac
  case "$1" in
    *.fish) scan_syntax='fish' scan_lines="$(own_fish_lines @DIR@)" scan_anywhere='' ;;
    *)
      scan_syntax='sh' scan_lines="$(own_sh_lines @DIR@)"
      scan_anywhere="$(guarded_line @DIR@; echo; printf '%s' "$nonlogin_prefix"; guarded_line @DIR@)"
      ;;
  esac
  scan_dir="$dest" scan_rel="$dest_rel" scan_syntax="$scan_syntax" scan_end="${scan_end:-}" \
  scan_lines="$scan_lines" scan_anywhere="$scan_anywhere" awk "$startup_awk"'
    BEGIN {
      stanza = 0
      found = 0
      prev = ""
      fdepth = 0; fended = 0; flost = 0; fcont = 0; fop = 0; fbs = 0
      fish = (ENVIRON["scan_syntax"] == "fish")
      n = split(ENVIRON["scan_lines"], list, "\n")
      for (i = 1; i <= n; i++) own[list[i]] = 1
      n = split(ENVIRON["scan_anywhere"], list, "\n")
      for (i = 1; i <= n; i++) if (list[i] != "") anywhere[list[i]] = 1
    }
    # fish: a command starts at the start of a line and after a ;, & or |
    # (so also after && and ||) that is not written with a backslash, and
    # its first word is the one after any and, or, not, !, command, builtin
    # or assignment (FOO=1). Blocks open with if, for, while, function,
    # switch or begin where a command starts and close with end. After a
    # return, exit or exec where a command starts (status is-login ||
    # return, not status is-interactive && exit), even in a block, no line
    # counts (fended). A line with a quote or a # that names one of those
    # words anywhere is not followed (flost), so nothing after it counts.
    function fish_track(s,    i, n, c, nparts, part, k, w) {
      if (s ~ /^#/) return
      if ((index(s, "\"") || index(s, sq) || index(s, "#")) && s ~ /(^|[ \t;(&|])(if|for|while|function|switch|begin|end|return|exit|exec)([ \t;)&|]|$)/) {
        flost = 1
        return
      }
      nparts = 1
      part[1] = ""
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "\\") { part[nparts] = part[nparts] substr(s, i, 2); i++ }
        else if (c == ";" || c == "&" || c == "|") part[++nparts] = ""
        else part[nparts] = part[nparts] c
      }
      for (k = 1; k <= nparts; k++) {
        w = trim(part[k])
        while (w ~ /^(and|or|not|!|command|builtin)([ \t]|$)/ || w ~ /^[A-Za-z_][A-Za-z0-9_]*=[^ \t]*[ \t]/) {
          sub(/^[^ \t]+/, "", w)
          w = trim(w)
        }
        sub(/[ \t].*/, "", w)
        if (w ~ /^(if|for|while|function|switch|begin)$/) fdepth++
        else if (w == "end") { if (fdepth > 0) fdepth--; else flost = 1 }
        else if (w == "return" || w == "exit" || w == "exec") fended = 1
      }
    }
    # The line with each way of writing the directory as @DIR@.
    function dir(s) {
      s = swap(s, ENVIRON["scan_dir"], "@DIR@")
      if (ENVIRON["scan_rel"] != "") {
        s = swap(s, "$HOME/" ENVIRON["scan_rel"], "@DIR@")
        if (!fish) s = swap(s, "${HOME}/" ENVIRON["scan_rel"], "@DIR@")
      }
      return s
    }
    {
      line = trim(dir($0))
      if ((line in own && prev == "# Added by the Axocoatl installer") || line in anywhere) {
        found = 1
        exit
      }
      if (fish) {
        if (line in own && !fdepth && !fended && !flost && !fcont) { found = 1; exit }
        fish_track(line)
        # fish goes on after a line that ends in \\ past comment lines,
        # until a blank line (fish 3.7), and after a &&, || or | (also one
        # that a comment follows) past blank and comment lines.
        if (line == "") { fbs = 0; fcont = fop }
        else if (line ~ /^#/) fcont = fop || fbs
        else {
          fop = (line ~ /(&&|\|)[ \t]*(\\|#.*)?$/)
          fbs = (line ~ /\\$/)
          fcont = fop || fbs
        }
        prev = line
        next
      }
      if (line in own && sure()) { found = 1; exit }
      prev = line
      if (stanza == 2 && line == "fi" && !lost && !ended) { found = 1; exit }
      if (stanza == 1 && line ~ /^(export[ \t]+)?PATH="?@DIR@:\$\{?PATH\}?"?$/) stanza = 2
      else stanza = 0
      if (stanza == 0 && sure() && line ~ /^if[ \t]+\[[ \t]+-d[ \t]+"?@DIR@"?[ \t]+\][ \t]*;[ \t]*then$/) stanza = 1
      track($0)
    }
    END {
      if (ENVIRON["scan_end"] == "1") {
        # (A file that adds the directory already gets no line.)
        if (found) exit 0
        if (fish) exit !(!fdepth && !fended && !flost && !fcont)
        exit !(!lost && !stopped && !nctx && !nblk && !cont && !pending && !testmode && !fname && !fnparen && !afterfname)
      }
      exit !found
    }
  ' "$1" 2>/dev/null
}

# Whether a line appended at the end of startup file $1 surely runs in every
# new terminal: no block is left open, the reader did not lose track, and the
# last line does not go on; in a fish file, no return, exit or exec came
# before it (as in `status is-login || return`); in an sh, bash or zsh file,
# none stopped it (startup_awk: not one in a function or a ( ) subshell, an
# exec with only redirections, nor one that stops only a shell that is not
# interactive, as Ubuntu's ~/.bashrc does).
appended_line_runs() {
  [ -f "$1" ] || return 0
  scan_end=1
  adds_to_path "$1"
  end_status=$?
  scan_end=""
  return "$end_status"
}

# Whether bash login file $1 (~/.bash_profile or ~/.bash_login) counts as
# reading ~/.profile (written ~/.profile, $HOME/.profile, ${HOME}/.profile,
# with double quotes around all or part of it, or its full path; not with a
# quote right before or after the ~, which then is not the home directory).
# It does with `. FILE` or `source FILE` alone, after `[ -f FILE ] &&` (or
# [[ ]], test, -r, -e, -s), or as `if [ -f FILE ]; then . FILE; fi`, on a
# line sure() is true for. It also does with `. FILE` alone in the then
# branch of a multi-line `if [ -f FILE ]; then` that starts on such a line:
# before any else or elif of that same if (each block the reader opens has its
# own number, so an if that a later `fi; if` or `done; if` opens is another
# one), on a line for which sure() would be true but for that one open if
# (not after a line that ends in && or \, nor inside another block), and only
# when that if then ends with `fi` alone on its line or followed only by
# redirections (`fi >/dev/null 2>&1`), so that it runs in this shell (not
# `fi | cmd` or `fi &`, which run it in a subshell). Each line is matched
# without the comment that track() finds on it, so a quoted # ("#") is not
# taken as one. The reader's limits (startup_awk) apply here too.
reads_dot_profile() {
  [ -f "$1" ] || return 1
  scan_profile="$HOME/.profile" awk "$startup_awk"'
    BEGIN {
      p = "(~|\\$HOME)/\\.profile"
      src = "(\\.|source)[ \t]+" p
      exists = "(\\[[ \t]+-[efrs][ \t]+" p "[ \t]+\\]|\\[\\[[ \t]+-[efrs][ \t]+" p "[ \t]+\\]\\]|test[ \t]+-[efrs][ \t]+" p ")"
      tail = "[ \t]*;?[ \t]*$"
      # Redirections after a compound command, which still runs in this shell.
      redirs = "([ \t]*[0-9]*(>|>>|<|>&|<&|&>|&>>|>\\|)[ \t]*[^ \t;&|<>()]+)*"
      alone = "^" src tail
      guarded = "^" exists "[ \t]*&&[ \t]*" src tail
      one_if = "^if[ \t]+" exists "[ \t]*;[ \t]*then[ \t]+" src "[ \t]*;[ \t]*fi" redirs tail
      open_if = "^if[ \t]+" exists "[ \t]*;[ \t]*then" tail
      close_if = "^fi" redirs tail
      abs = ENVIRON["scan_profile"]
      # opened: the number of the multi-line if that tests ~/.profile, while
      # it is open; sourced: the same, once its then branch reads ~/.profile.
      opened = 0
      sourced = 0
      found = 0
    }
    {
      top = sure()
      cur = (nblk == 1) ? bid[1] : 0
      in_then = opened && cur == opened && !branched && blk[1] == "if"
      in_then = in_then && !lost && !ended && !nctx && !cont && !pending && !testmode && !fname && !fnparen && !afterfname
      was = nblk
      track($0)
      line = cmtat ? substr($0, 1, cmtat - 1) : $0
      line = swap(line, sq abs sq, "$HOME/.profile")
      line = swap(line, abs, "$HOME/.profile")
      line = swap(line, "${HOME}", "$HOME")
      # A quoted ~, or a ~ right before a quote, is not the home directory;
      # other double quotes go.
      gsub("[\"" sq "]~|~[\"" sq "]", "@QUOTED@", line)
      gsub(/"/, "", line)
      line = trim(line)
      if (top && (line ~ alone || line ~ guarded || line ~ one_if)) { found = 1; exit }
      if (in_then && line ~ alone) sourced = opened
      if (top && was == 0 && nblk == 1) { opened = (line ~ open_if) ? bid[1] : 0; sourced = 0; branched = 0 }
      if (opened && cur == opened && (nblk == 0 || bid[1] != opened)) {
        # That if ended on this line.
        if (sourced == opened && line ~ close_if && nblk == 0 && !lost) { found = 1; exit }
        opened = 0
        sourced = 0
      }
    }
    END { exit !found }
  ' "$1" 2>/dev/null
}

# The startup files a new shell of the user's login shell reads, one per
# line, and the line that puts $1 on PATH in them. Sets profiles,
# path_line (for those files), now_line (for this shell) and login_covers
# (a bash login file that puts $1 on PATH already, or empty).
# Terminal on macOS starts login shells. On Linux a new terminal window
# starts an interactive shell (.bashrc, .zshrc) and a new login (SSH, the
# console, the desktop session) a login shell, which reads only its login
# files (.bash_profile, .bash_login or .profile; .zprofile), so the line goes
# into both. The line adds the directory only when PATH does not have it
# yet: a login zsh reads .zprofile and then .zshrc, and a login bash's
# .profile or .bash_profile often reads .bashrc, and neither should list it
# twice.
choose_profiles() {
  case "$1" in
    "$HOME"/*) dir_expr="\$HOME/${1#"$HOME"/}" ;;
    *) dir_expr="$1" ;;
  esac
  user_shell="${SHELL:-}"
  user_shell="${user_shell##*/}"
  now_line="$(export_line "$dir_expr")"
  path_line="$(guarded_line "$dir_expr")"
  login_covers=""
  case "$user_shell" in
    zsh)
      if [ "$os" = "Darwin" ]; then
        profiles="${ZDOTDIR:-$HOME}/.zprofile"
      else
        profiles="${ZDOTDIR:-$HOME}/.zshrc
${ZDOTDIR:-$HOME}/.zprofile"
      fi
      ;;
    bash)
      # A login bash reads only the first of these that exists, so a new
      # .bash_profile would hide an existing .profile.
      login_file=""
      for candidate in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
        if [ -f "$candidate" ]; then
          login_file="$candidate"
          break
        fi
      done
      if [ "$os" = "Darwin" ]; then
        profiles="${login_file:-$HOME/.bash_profile}"
      else
        login_file="${login_file:-$HOME/.profile}"
        if adds_to_path "$login_file"; then
          login_covers="$login_file"
        elif [ "$login_file" != "$HOME/.profile" ] && reads_dot_profile "$login_file" \
          && adds_to_path "$HOME/.profile"; then
          login_covers="$HOME/.profile"
        fi
        if [ -n "$login_covers" ]; then
          # Ubuntu's and Debian's ~/.profile read ~/.bashrc and then add
          # ~/.local/bin whether or not PATH has it, so a line in ~/.bashrc
          # that ran in a login shell would list it twice: there it runs
          # only in a shell that is not a login shell (a new terminal
          # window whose session started before ~/.local/bin existed).
          profiles="$HOME/.bashrc"
          path_line="${nonlogin_prefix}${path_line}"
        else
          profiles="$HOME/.bashrc
${login_file}"
        fi
      fi
      ;;
    fish)
      # fish reads config.fish in every shell, login or not, and
      # fish_add_path adds nothing PATH already has; with --path it changes
      # only this shell's PATH and saves nothing (fish 3.2 or later).
      profiles="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
      path_line="$(fish_line "$dir_expr")"
      now_line=$path_line
      ;;
    *)
      profiles="$HOME/.profile"
      ;;
  esac
}

# Paths, one per line, as the user knows them: "~/.a" or "~/.a and ~/.b".
display_paths() {
  shown=""
  old_ifs=$IFS
  IFS='
'
  set -f
  for each in $1; do
    shown="${shown}${shown:+ and }$(display_path "$each")"
  done
  set +f
  IFS=$old_ifs
  printf '%s' "$shown"
}

# Whether a question can be asked: the terminal opens for reading and writing.
can_ask() {
  ( exec <"$tty_device" >>"$tty_device" ) 2>/dev/null
}

require_supported_glibc() {
  [ "$os" = "Linux" ] || return 0
  command -v getconf >/dev/null 2>&1 \
    || err "prebuilt Linux releases require GNU libc 2.35 or newer; this system cannot report its libc version. Alpine/musl needs a source build"

  libc_version="$(getconf GNU_LIBC_VERSION 2>/dev/null || true)"
  case "$libc_version" in
    glibc\ *) version="${libc_version#glibc }" ;;
    *) err "prebuilt Linux releases require GNU libc 2.35 or newer; this system reported '$libc_version'. Alpine/musl needs a source build" ;;
  esac

  major="${version%%.*}"
  remainder="${version#*.}"
  [ "$remainder" != "$version" ] || err "could not parse GNU libc version '$version'"
  minor="${remainder%%.*}"
  case "$major" in ''|*[!0-9]*) err "could not parse GNU libc version '$version'" ;; esac
  case "$minor" in ''|*[!0-9]*) err "could not parse GNU libc version '$version'" ;; esac

  if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 35 ]; }; then
    err "prebuilt Linux releases require GNU libc 2.35 or newer; detected $libc_version. Upgrade the distribution or build from source"
  fi
}

require_supported_macos() {
  [ "$os" = "Darwin" ] || return 0
  command -v sw_vers >/dev/null 2>&1 \
    || err "prebuilt macOS releases require macOS 11 or newer; this system cannot report its macOS version"

  macos_version="$(SYSTEM_VERSION_COMPAT=0 sw_vers -productVersion 2>/dev/null || true)"
  [ -n "$macos_version" ] \
    || err "prebuilt macOS releases require macOS 11 or newer; this system cannot report its macOS version"
  macos_major="${macos_version%%.*}"
  case "$macos_major" in
    ''|*[!0-9]*) err "could not parse macOS version '$macos_version'" ;;
  esac

  if [ "$macos_major" -lt 11 ]; then
    err "prebuilt macOS releases require macOS 11 or newer; detected macOS $macos_version. Upgrade macOS before installing this release"
  fi
}

# --- Detect OS / arch -> release target triple ---
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux)  os_part="unknown-linux-gnu" ;;
  Darwin) os_part="apple-darwin" ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    err "Axocoatl runs on Windows inside WSL2, not natively (its session sandbox
  is Podman and its service is systemd/launchd); WSL2 is not yet verified. Open a
  WSL2 distro (e.g. Ubuntu) and run this same command there:

      curl -fsSL https://axocoatl.ai/install.sh | sh

  No WSL2 yet? In an admin PowerShell:  wsl --install  (then reboot).
  Full guide: https://docs.axocoatl.ai/start/install/#windows-through-wsl2" ;;
  *) err "unsupported OS '$os' — use 'cargo install axocoatl-cli' or build from source" ;;
esac

case "$arch" in
  x86_64|amd64)  arch_part="x86_64" ;;
  arm64|aarch64) arch_part="aarch64" ;;
  *) err "unsupported architecture '$arch'" ;;
esac

require_supported_glibc
require_supported_macos
target="${arch_part}-${os_part}"

# --- Resolve the release tag ---
if [ -n "$requested_tag" ]; then
  tag="$requested_tag"
  info "installing the requested release ${tag}..."
else
  info "resolving latest release..."
  tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | head -n1 | cut -d'"' -f4)"
  # GitHub answers at most 60 requests an hour from one address without a
  # token, which a shared office or CI address can use up.
  [ -n "$tag" ] || err "could not resolve the latest release from api.github.com (it answers at most 60 requests an hour from one address without a token); set AXOCOATL_VERSION to a release tag, such as v1.5.0, to install that release without asking it"
fi

tarball="${BIN}-${tag}-${target}.tar.gz"
url="${base_url:-https://github.com/${REPO}/releases/download/${tag}}/${tarball}"
sha_url="${url}.sha256"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

info "downloading ${tarball} (${tag})..."
# This shell writes the downloads, not curl: the curl snap (what Ubuntu offers
# when curl is missing) has a /tmp of its own, so `-o` into this directory
# fails with "(23) client returned ERROR on write".
curl -fsSL "$url" > "${tmp}/${tarball}" || err "download failed: $url"

# --- Require and verify the release checksum ---
curl -fsSL "$sha_url" > "${tmp}/sha" \
  || err "checksum download failed: $sha_url"

if ! awk -v expected_name="$tarball" '
  NR != 1 { exit 1 }
  NF != 2 { exit 1 }
  length($1) != 64 { exit 1 }
  $1 !~ /^[0-9A-Fa-f]+$/ { exit 1 }
  $2 != expected_name { exit 1 }
  END { if (NR != 1) exit 1 }
' "${tmp}/sha"; then
  err "malformed checksum file for $tarball"
fi

expected="$(awk '{print $1}' "${tmp}/sha" | tr 'A-F' 'a-f')"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${tmp}/${tarball}" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "${tmp}/${tarball}" | awk '{print $1}')"
else
  err "neither sha256sum nor shasum is available"
fi
actual="$(printf '%s' "$actual" | tr 'A-F' 'a-f')"
[ "$expected" = "$actual" ] \
  || err "checksum mismatch (expected $expected, got $actual)"
info "checksum verified"

tar -xzf "${tmp}/${tarball}" -C "$tmp"

# --- Choose install dir ---
if [ -w "$system_dir" ]; then
  dest="$system_dir"
else
  dest="${HOME}/.local/bin"
  mkdir -p "$dest"
fi

# --- What this install replaces ---
# The Axocoatl at the path it writes, and any daemon (`axocoatl serve`, which
# the Always-On Service runs, or `axocoatl dev`) that runs now: a daemon keeps
# running the binary it started from until it restarts, and only a started
# daemon of the new version changes its data.
previous=""
if [ -x "${dest}/${BIN}" ]; then
  previous="$("${dest}/${BIN}" --version 2>/dev/null | head -n 1 | awk '{print $2}')" || previous=""
  case "$previous" in
    ''|*[!0-9.]*) previous="" ;;
  esac
fi
# A daemon is `axocoatl serve` or `axocoatl dev` run by name or by a path,
# which may hold spaces (a home folder's name can). The program's path is
# words up to `/axocoatl`, none of them an option.
daemons="$(ps -A -o args= 2>/dev/null | awk -v bin="$BIN" '
  BEGIN { daemon = "^((/[^ ]*( [^ /-][^ ]*)*)/)?" bin " (serve|dev) " }
  match($0 " ", daemon) { print substr($0 " ", RSTART, RLENGTH - 1) }
' || true)"
[ -z "$previous" ] || info "replacing Axocoatl ${previous} at ${dest}/${BIN}"

install -m 0755 "${tmp}/${BIN}" "${dest}/${BIN}" 2>/dev/null \
  || { cp "${tmp}/${BIN}" "${dest}/${BIN}" && chmod 0755 "${dest}/${BIN}"; }

info "installed ${BIN} ${tag} -> ${dest}/${BIN}"
# A new minor or major release reads data the older one wrote, and the older
# one cannot always read it back: the way back is the backup. Going back is
# said as such: the older release may not start on what the newer one wrote.
new_version="${tag#v}"
direction=""
[ -z "$previous" ] || direction="$(awk -v old="$previous" -v new="$new_version" 'BEGIN {
  split(old, o, "."); split(new, n, ".")
  for (i = 1; i <= 2; i++) {
    if (o[i] + 0 < n[i] + 0) { print "upgrade"; exit }
    if (o[i] + 0 > n[i] + 0) { print "downgrade"; exit }
  }
}')"
case "$direction" in
  upgrade) info "upgrading from Axocoatl ${previous}: before Axocoatl starts again, back up its data and read https://docs.axocoatl.ai/operate/upgrade/ (going back to ${previous} needs that backup)" ;;
  downgrade) info "going back from Axocoatl ${previous} to ${new_version}: an older release may not start on data a newer one wrote. Before Axocoatl starts again, read how to go back at https://docs.axocoatl.ai/operate/upgrade/ (it uses the backup made before upgrading to ${previous})" ;;
esac
# A daemon of the copy just replaced (by its path, as the Always-On Service
# records it, or by name) runs the new one once it restarts; a daemon of
# another copy, such as a build from source, does not.
dest_real="$(CDPATH='' cd -P -- "$dest" 2>/dev/null && pwd)" || dest_real="$dest"
printf '%s\n' "$daemons" | while IFS= read -r daemon; do
  case "$daemon" in
    '') ;;
    "${dest}/${BIN} "*|"${dest_real}/${BIN} "*|"${BIN} "*)
      info "an Axocoatl daemon is running (${daemon}); it runs the binary it started from until it restarts. Restart it: '${BIN} service restart' for the Always-On Service, or stop '${BIN} dev' (Ctrl-C) and start it again" ;;
    *)
      info "an Axocoatl daemon runs another copy (${daemon}); this install does not change it. To run ${tag} instead, stop it and start ${dest}/${BIN} (for the Always-On Service: '${dest}/${BIN} service install', then '${BIN} service restart')" ;;
  esac
done

# --- Keep it on PATH for new terminals ---
# A stock macOS PATH (/etc/paths) has no ~/.local/bin, and an `export` typed
# in this shell is gone in the next Terminal window.
offer_profile_line() {
  choose_profiles "$dest"
  shown_dest="$(display_path "$dest")"
  # The startup files that do not put it on PATH yet, and those that do.
  missing=""
  present="$login_covers"
  old_ifs=$IFS
  IFS='
'
  set -f
  for profile in $profiles; do
    if adds_to_path "$profile"; then
      present="${present}${present:+
}${profile}"
    else
      missing="${missing}${missing:+
}${profile}"
    fi
  done
  set +f
  IFS=$old_ifs
  case "$present" in
    '') ;;
    *'
'*) present_note="$(display_paths "$present") already add ${shown_dest} to PATH" ;;
    *) present_note="$(display_paths "$present") already adds ${shown_dest} to PATH" ;;
  esac
  if [ -z "$missing" ]; then
    info "${present_note}; new terminals find ${BIN}"
    return 0
  fi
  if [ -n "$login_covers" ] && [ "$present" = "$login_covers" ]; then
    info "${present_note} in a new login"
  elif [ -n "$present" ]; then
    info "${present_note}"
  fi
  answer=no
  if [ "$modify_path" = "yes" ]; then
    answer=yes
  elif [ "$modify_path" = "ask" ] && can_ask; then
    printf 'axocoatl-install: %s is not on your PATH. Add it in %s? [Y/n] ' \
      "$shown_dest" "$(display_paths "$missing")" >>"$tty_device"
    reply=""
    if read -r reply <"$tty_device"; then
      case "$reply" in
        ''|y|Y|yes|Yes|YES) answer=yes ;;
        *) answer=no ;;
      esac
    else
      # End of input (Ctrl-D, or a terminal that went away) is not consent.
      printf '\n' 2>/dev/null >>"$tty_device" || true
    fi
  fi
  if [ "$answer" = "yes" ]; then
    written=""
    failed=""
    stops=""
    old_ifs=$IFS
    IFS='
'
    set -f
    for profile in $missing; do
      appended_line_runs "$profile" || stops="${stops}${stops:+
}${profile}"
      if mkdir -p "${profile%/*}" 2>/dev/null \
        && printf '\n# Added by the Axocoatl installer\n%s\n' "$path_line" 2>/dev/null >>"$profile"; then
        written="${written}${written:+
}${profile}"
      else
        failed="${failed}${failed:+
}${profile}"
      fi
    done
    set +f
    IFS=$old_ifs
    [ -z "$written" ] || info "added to $(display_paths "$written"):  ${path_line}"
    if [ -n "$stops" ]; then
      info "$(display_paths "$stops") can stop before that line in some shells (a return, exit or exec, or a line the installer cannot follow, comes first); if a new terminal does not find ${BIN}, move the line above it"
    fi
    if [ -z "$failed" ]; then
      if [ -z "$stops" ]; then
        info "new terminals find ${BIN}; in this one, run:  ${now_line}"
      else
        info "in this terminal, run:  ${now_line}"
      fi
      return 0
    fi
    info "could not write $(display_paths "$failed")"
    missing=$failed
  fi
  info "${shown_dest} is not on your PATH. For new terminals to find ${BIN}, add this line to $(display_paths "$missing"):"
  echo
  echo "    ${path_line}"
}

case ":${PATH}:" in
  *":${dest}:"*) ;;
  *)
    if [ -n "${GITHUB_PATH:-}" ] && [ "$modify_path" != "no" ]; then
      # A GitHub Actions step: the job's later steps read $GITHUB_PATH, and
      # there is no profile or new terminal to set up.
      if printf '%s\n' "$dest" 2>/dev/null >>"$GITHUB_PATH"; then
        info "added ${dest} to GITHUB_PATH; the job's later steps find ${BIN}"
      else
        info "could not write GITHUB_PATH; add ${dest} to PATH in later steps"
      fi
    else
      offer_profile_line
    fi
    ;;
esac

# The next commands must run as printed: by name only when this shell
# resolves the name to the binary just installed.
found="$(command -v "$BIN" 2>/dev/null || true)"
if [ "$found" = "${dest}/${BIN}" ]; then
  run="$BIN"
else
  run="$(shell_word "${dest}/${BIN}")"
  if [ -n "$found" ]; then
    info "note: ${found} comes first on your PATH, so '${BIN}' runs that copy; the commands below name the one just installed"
  fi
fi

# Every Session and run needs rootless Podman, which `axocoatl dev` finds on
# PATH. Onboarding looks for programs on PATH and then in podman_dirs, the
# same list as extra_dirs in axocoatl-cli/src/onboard/host.rs (Host::from_env;
# scripts/test-install.sh checks that they match). When it finds no podman
# there, it installs Podman after asking where it can, and otherwise says what
# to do (onboard/sandbox.rs, install). When it finds one only in such a
# directory, it installs nothing and does not change PATH (sandbox.rs,
# on_path).
if ! command -v podman >/dev/null 2>&1; then
  if [ "$os" = "Darwin" ]; then
    podman_dirs='/opt/homebrew/bin /usr/local/bin /opt/podman/bin /Applications/Ollama.app/Contents/Resources'
    podman_dirs_shown='/opt/homebrew/bin, /usr/local/bin or /opt/podman/bin'
  else
    podman_dirs='/usr/local/bin'
    podman_dirs_shown='/usr/local/bin'
  fi
  podman_found=""
  for podman_dir in $podman_dirs; do
    if [ -f "${root_dir}${podman_dir}/podman" ] && [ -x "${root_dir}${podman_dir}/podman" ]; then
      podman_found="${podman_dir}/podman"
      break
    fi
  done
  if [ -n "$podman_found" ]; then
    info "Podman, which every Session and run needs, is installed at ${podman_found}, but ${podman_dir} is not on PATH, where axocoatl dev looks for it."
    info "Add ${podman_dir} to PATH for new terminals; onboard (below) finds Podman there, but does not change PATH."
  else
    info "Podman, which every Session and run needs, is not on PATH or in ${podman_dirs_shown}."
    info "onboard (below) installs it after asking, with Homebrew on an Apple-silicon Mac or with apt, dnf, pacman or zypper on Linux. On an Apple-silicon Mac without Homebrew it points to the installer from podman.io; on an Intel Mac, on Ubuntu 22.04 or older, or on another Linux it says what to do."
  fi
fi

echo
echo "Next:  ${run} onboard      # set up a writer and a reviewer, and Podman where it can (else it says how); it asks first"
echo "       ${run} doctor       # check the sandbox, the models and the daemon"
echo "       ${run} dev          # start the daemon; leave it running"
