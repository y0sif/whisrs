#!/usr/bin/env bash
# Docs check (CI runs it): keeps markdown to the allowed paths, CLAUDE.md files to one import,
# private data out of every file, history and provenance out of the user docs, and each doc
# under its line budget. Prints one `path:line: rule: excerpt` line per violation, then a
# count, and exits 1 when there is any. Untracked files that are not ignored are checked too,
# so an internal file fails here before it is committed. The generated
# docs/config-reference.md has its own freshness test in `cargo test`.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# Git with paths printed as they are: a quoted non-ASCII name would match no rule.
git() { command git -c core.quotePath=false "$@"; }

violations=0

# report PATH LINE RULE EXCERPT
report() {
  local excerpt=$4
  excerpt=${excerpt#"${excerpt%%[![:space:]]*}"}
  printf '%s:%s: %s: %s\n' "$1" "$2" "$3" "${excerpt:0:120}"
  violations=$((violations + 1))
}

# report_hits RULE, reading `path:line:text` lines (grep -nH, git grep -n) from stdin
report_hits() {
  local hit path line
  while IFS= read -r hit; do
    path=${hit%%:*}
    hit=${hit#*:}
    line=${hit%%:*}
    report "$path" "$line" "$1" "${hit#*:}"
  done
}

# Tracked files and untracked ones that are not ignored, as long as they exist on disk. File
# lists are NUL-separated from here on, so every name is read whole.
mapfile -d '' files < <(
  git ls-files -z --cached --others --exclude-standard | sort -zu | while IFS= read -r -d '' f; do
    if [[ -f $f ]]; then printf '%s\0' "$f"; fi
  done
)

# matching REGEX: the files whose path matches the extended regex, NUL-separated
matching() {
  local f
  for f in "${files[@]}"; do
    if [[ $f =~ $1 ]]; then printf '%s\0' "$f"; fi
  done
}

# 1. Markdown lives only at these paths. Specs, plans, notes and other internal files go
#    under .claude/ (ignored), never in the repo.
allowed='^(README|CONTRIBUTING)\.md$|^(.+/)?(AGENTS|CLAUDE)\.md$|^docs/[^/]+\.md$'
allowed+='|^crates/[^/]+/README\.md$|^contrib/(.+/)?README\.md$'
allowed+='|^contrib/agent-skill/(.+/)?SKILL\.md$|^src/overlay/font/README\.md$|^\.github/(.+/)?[^/]+\.md$'
while IFS= read -r -d '' f; do
  [[ $f =~ $allowed ]] || report "$f" 1 allowlist "not an allowed docs path; internal files go under .claude/"
done < <(matching '\.md$')

# 2. A CLAUDE.md holds only `@AGENTS.md`: the guide itself is AGENTS.md.
only_import='^[[:space:]]*@AGENTS\.md[[:space:]]*$'
while IFS= read -r -d '' f; do
  report_hits claude-md < <(grep -nHv -e '^[[:space:]]*$' -e "$only_import" -- "$f")
  grep -q -e "$only_import" -- "$f" || report "$f" 1 claude-md "missing @AGENTS.md"
done < <(matching '(^|/)CLAUDE\.md$')

# 3. Private data, in every text file.
# Home directories other than the placeholder users tests and examples use.
while IFS= read -r hit; do
  [[ ${hit##*:} =~ ^/home/(user|u|ada)$ ]] || report_hits private-path <<<"$hit"
done < <(git grep -nIoE --untracked '/home/[a-z][A-Za-z0-9_.-]*')
report_hits private-link < <(git grep -nIE --untracked 'claude\.ai/code/session')
# Email addresses. Allowed: noreply ones, reserved example domains, and names that only look
# like an address: hi-dpi images (scene@2x.png), systemd template units (user@1000.service,
# getty@tty1.service), SSH remotes (git@github.com, aur@aur.archlinux.org), the GNOME
# extension's UUID, and URL userinfo in a host-matching test (src/config/catalog.rs).
# Cargo.toml has no authors field, so it adds none.
email_ok='noreply|@example\.(com|org|net)$|@[0-9]+x\.png$|@[^@]+\.service$|^(git|aur)@'
email_ok+='|^whisrs-overlay@eresende\.github$|^user@openrouter\.ai$'
while IFS= read -r hit; do
  [[ ${hit#*:*:} =~ $email_ok ]] || report_hits private-email <<<"$hit"
done < <(git grep -nIoE --untracked '[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}')

# 4. User docs: no em or en dashes, no test provenance, no fix history (those go in the
#    commit).
user_docs='^(README|CONTRIBUTING)\.md$|^docs/[^/]+\.md$|^crates/[^/]+/README\.md$'
user_docs+='|^contrib/(.+/)?README\.md$|^contrib/[^/]+\.1$'
mapfile -d '' docs < <(matching "$user_docs" | grep -zvxF docs/config-reference.md)

# grep_docs RULE GREP-OPTIONS...
grep_docs() {
  local rule=$1
  shift
  if ((${#docs[@]})); then
    report_hits "$rule" < <(grep -nH "$@" -- "${docs[@]}")
  fi
}
# U+2014 and U+2013, and roff's spellings of them in the man pages.
grep_docs dash -F -e $'\xe2\x80\x94' -e '\(em' -e '\[em]' -e $'\xe2\x80\x93' -e '\(en' -e '\[en]'
# "measured on" only as a measurement of the maintainer's, not "a level measured on a track".
provenance='\(measured\)|measured on (a|my|our|his|this) |tested against|tested on [0-9]|mock model'
grep_docs provenance -iE "$provenance"
grep_docs history -iE '\b(since|until|in|see|fixes|closes|issue|pr) #[0-9]+|\(#[0-9]+\)|as in [0-9]+\.[0-9]+\.[0-9]+'

# 5. Line budgets. Cut before adding. docs/configuration.md is the hub page that links out;
#    docs/config-reference.md is generated, so exempt.
# budget FILE LIMIT
budget() {
  local n
  n=$(wc -l <"$1")
  ((n <= $2)) || report "$1" $(($2 + 1)) budget "$n lines, the limit is $2"
}
while IFS= read -r -d '' f; do
  case $f in
  README.md) budget "$f" 220 ;;
  AGENTS.md) budget "$f" 150 ;;
  */AGENTS.md) budget "$f" 60 ;;
  docs/config-reference.md) ;;
  docs/configuration.md) budget "$f" 500 ;;
  docs/*.md) budget "$f" 300 ;;
  esac
done < <(matching '^(README\.md|(.+/)?AGENTS\.md|docs/[^/]+\.md)$')

if ((violations)); then
  echo "check-docs: $violations violation(s)"
  exit 1
fi
echo "check-docs: ok"
