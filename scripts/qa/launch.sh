#!/bin/zsh
# Builds the debug app and launches it with a throwaway HOME, optionally opening a project in it.
# Usage: scripts/qa/launch.sh [project-path]
set -euo pipefail
cd "${0:a:h}/../.."

project=
if (( $# > 0 )); then
  project=${1:A}
  [[ -d $project ]] || { echo "launch.sh: $1 is not a folder" >&2; exit 2; }
fi

cargo build -p athena -p athena-mux

qa_home=$(mktemp -d /tmp/athena-qa.XXXXXX)
HOME=$qa_home target/debug/athena >"$qa_home/stdout.log" 2>&1 &
pid=$!

# With an empty HOME the CLI would hand a folder argument to the installed app, so the project
# goes to this window over its own socket once that exists.
if [[ -n $project ]]; then
  socket="$qa_home/Library/Application Support/athena/app.sock"
  for _ in {1..100}; do
    [[ -S $socket ]] && break
    kill -0 $pid 2>/dev/null || { echo "launch.sh: athena exited, see $qa_home/stdout.log" >&2; exit 1; }
    sleep 0.2
  done
  HOME=$qa_home target/debug/athena "$project"
fi

echo "pid:  $pid"
echo "HOME: $qa_home"
