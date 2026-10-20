#!/bin/bash
# Crash check for interrupted receives. Shares a file (1 GiB, or SIZE_MIB) from one data directory to
# another, kills the receiver with SIGKILL partway through, resumes it in a new process,
# and checks that the saved file matches the source. Run it after `cargo build --release -p groovy-cli`.
set -u
# Use the release build for large sizes (cargo build --release -p groovy-cli): a debug build
# is many times slower.
CLI="${CLI:-$(cd "$(dirname "$0")/.." && pwd)/target/release/groovy-cli}"
# Work in target/, which has room for the 1 GiB file, and is removed at the end.
mkdir -p "$(dirname "$0")/../target"
T=$(mktemp -d "$(cd "$(dirname "$0")/.." && pwd)/target/crash-check.XXXX")
export DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent  # file identity, not the real keyring
mkdir -p $T/src $T/out
SIZE=${SIZE_MIB:-1024}
head -c $((SIZE * 1024 * 1024)) /dev/urandom > $T/src/big.bin
SRC_HASH=$(sha256sum $T/src/big.bin | cut -d' ' -f1)
GROOVY_DATA_DIR=$T/send $CLI send $T/src/big.bin > $T/send.log 2>&1 &
SENDER=$!
for i in $(seq 1 120); do grep -q "share ticket:" $T/send.log 2>/dev/null && break; sleep 1; done
TICKET=$(grep "share ticket:" $T/send.log | sed 's/share ticket: //')
GROOVY_DATA_DIR=$T/recv $CLI receive "$TICKET" --out $T/out > $T/recv1.log 2>&1 &
RECEIVER=$!
sleep 6
kill -9 $RECEIVER; wait $RECEIVER 2>/dev/null
echo "killed the receiver with SIGKILL after 6 s, of $SIZE MiB"
GROOVY_DATA_DIR=$T/recv $CLI resume --out $T/out > $T/resume.log 2>&1
echo "resume exit: $?"
cat $T/resume.log
if [ -f $T/out/big.bin ]; then
  OUT_HASH=$(sha256sum $T/out/big.bin | cut -d' ' -f1)
  if [ "$OUT_HASH" = "$SRC_HASH" ]; then echo "file matches the source"; echo pass > "$T/result"; else echo "FILE DIFFERS"; fi
else
  echo "no file saved"
fi
kill -INT $SENDER 2>/dev/null; wait $SENDER 2>/dev/null
echo "work folder: $T (kept only if the check failed)"

# The work folder is removed when the check passes, and kept when it fails so it can be looked at.
if [ "$(cat "$T/result" 2>/dev/null)" = "pass" ]; then rm -rf "$T"; fi
