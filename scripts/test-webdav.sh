#!/usr/bin/env bash
#
# End-to-end WebDAV test against a running r2-webdav server backed by real R2.
#
# Requires these env vars (same ones the server uses):
#   WEBDAV_USERNAME, WEBDAV_PASSWORD
# Optional:
#   BASE_URL   (default http://localhost:4918)
#   SIZE_MB    (test file size; default 50 -> exercises multipart upload)
#
# The server must already be running separately, e.g.:
#   ./target/release/r2-webdav
#
set -euo pipefail

BASE_URL="${BASE_URL:-http://localhost:4918}"
: "${WEBDAV_USERNAME:?WEBDAV_USERNAME is not set}"
: "${WEBDAV_PASSWORD:?WEBDAV_PASSWORD is not set}"
SIZE_MB="${SIZE_MB:-50}"
# Credentials are read from the environment; nothing is hardcoded here.
AUTH=(-u "${WEBDAV_USERNAME}:${WEBDAV_PASSWORD}")

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
SRC="$WORK/src.bin"
DST="$WORK/dst.bin"
PART="$WORK/part.bin"

pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; exit 1; }

# curl returning only the HTTP status code
code() { curl -sk "${AUTH[@]}" -o /dev/null -w '%{http_code}' "$@"; }

echo "==> Server: $BASE_URL   user: $WEBDAV_USERNAME   test size: ${SIZE_MB} MiB"

echo "==> Generating ${SIZE_MB} MiB random file"
dd if=/dev/urandom of="$SRC" bs=1M count="$SIZE_MB" status=none
SRC_SUM="$(sha256sum "$SRC" | cut -d' ' -f1)"

echo "==> 1. PUT (upload, multipart if > 8 MiB)"
c=$(code -T "$SRC" "$BASE_URL/test-big.bin")
[[ "$c" =~ ^2 ]] && pass "upload -> $c" || fail "upload -> $c"

echo "==> 2. GET (download) + checksum"
curl -sk "${AUTH[@]}" -o "$DST" "$BASE_URL/test-big.bin"
DST_SUM="$(sha256sum "$DST" | cut -d' ' -f1)"
[[ "$SRC_SUM" == "$DST_SUM" ]] && pass "checksums match ($SRC_SUM)" \
  || fail "checksum mismatch: $SRC_SUM != $DST_SUM"

echo "==> 3. Range GET (bytes 0-1023)"
curl -sk "${AUTH[@]}" -r 0-1023 -o "$PART" "$BASE_URL/test-big.bin"
n=$(wc -c < "$PART")
head -c 1024 "$SRC" | cmp -s - "$PART" && [[ "$n" -eq 1024 ]] \
  && pass "got $n bytes, content matches" || fail "range mismatch (got $n bytes)"

echo "==> 4. MKCOL (create directory)"
c=$(code -X MKCOL "$BASE_URL/testdir/")
[[ "$c" =~ ^2 ]] && pass "mkcol -> $c" || fail "mkcol -> $c"

echo "==> 5. PUT small file into directory"
echo "hello r2 webdav" > "$WORK/hello.txt"
c=$(code -T "$WORK/hello.txt" "$BASE_URL/testdir/hello.txt")
[[ "$c" =~ ^2 ]] && pass "upload into dir -> $c" || fail "upload into dir -> $c"

echo "==> 6. PROPFIND Depth:1 on directory (listing)"
body="$(curl -sk "${AUTH[@]}" -X PROPFIND -H 'Depth: 1' "$BASE_URL/testdir/")"
grep -q "hello.txt" <<<"$body" && pass "listing contains hello.txt" \
  || fail "listing missing hello.txt"

echo "==> 7. MOVE (rename) hello.txt -> world.txt"
c=$(code -X MOVE -H "Destination: $BASE_URL/testdir/world.txt" "$BASE_URL/testdir/hello.txt")
[[ "$c" =~ ^2 ]] && pass "move -> $c" || fail "move -> $c"

echo "==> 8. COPY world.txt -> copy.txt"
c=$(code -X COPY -H "Destination: $BASE_URL/testdir/copy.txt" "$BASE_URL/testdir/world.txt")
[[ "$c" =~ ^2 ]] && pass "copy -> $c" || fail "copy -> $c"

echo "==> 9. DELETE test files and directory"
code -X DELETE "$BASE_URL/test-big.bin"   >/dev/null
code -X DELETE "$BASE_URL/testdir/"       >/dev/null
c=$(code "$BASE_URL/test-big.bin")
[[ "$c" == "404" ]] && pass "deleted file now 404" || fail "expected 404 after delete, got $c"

echo
printf '\033[32mAll WebDAV upload/download tests passed.\033[0m\n'
