#!/usr/bin/env bash
# After remote.sh: the ext4 traced round again (its first trace filter missed the writer's thread name).
until [ -f "$HOME/ckpt/DONE" ]; do sleep 10; done
FSLIST=ext4 FIRST_ROUND=3 ROUNDS=3 DONE_FILE=DONE2 LOG_FILE=rerun.log ENV_FILE=environment-rerun.txt bash "$HOME/scripts/remote2.sh"
