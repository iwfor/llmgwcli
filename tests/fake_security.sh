#!/bin/sh
# Fake macOS `security` command for tests, so they never touch the real keychain.
# State lives in $FAKE_SECURITY_DIR, one file per service/account.
# Every argv is logged to argv.log; `-i` stdin to stdin.log.
# Set FAKE_SECURITY_FAIL=1 to make every call fail.
dir="$FAKE_SECURITY_DIR"
mkdir -p "$dir"
echo "$*" >> "$dir/argv.log"

if [ -n "$FAKE_SECURITY_FAIL" ]; then
  echo "security: fake failure" >&2
  exit 1
fi

notfound() {
  echo "security: The specified item could not be found in the keychain." >&2
  exit 44
}

# Prints the value following option $1 in the remaining args.
opt() {
  want=$1
  shift
  while [ $# -gt 0 ]; do
    if [ "$1" = "$want" ]; then
      echo "$2"
      return
    fi
    shift
  done
}

if [ "$1" = "-i" ]; then
  # Only add-generic-password is sent this way: -s "svc" -a "acct" -w "secret"
  read -r line
  echo "$line" >> "$dir/stdin.log"
  svc=$(echo "$line" | sed -E 's/.* -s "([^"]*)".*/\1/')
  acct=$(echo "$line" | sed -E 's/.* -a "([^"]*)".*/\1/')
  secret=$(echo "$line" | sed -E 's/.* -w "(.*)"$/\1/')
  printf '%s' "$secret" > "$dir/$svc.$acct"
  exit 0
fi

cmd=$1
shift
file="$dir/$(opt -s "$@").$(opt -a "$@")"
case "$cmd" in
  find-generic-password)
    [ -f "$file" ] || notfound
    cat "$file"
    echo
    ;;
  delete-generic-password)
    [ -f "$file" ] || notfound
    rm "$file"
    echo "password has been deleted."
    ;;
  *)
    echo "fake security: unsupported command $cmd" >&2
    exit 2
    ;;
esac
