#!/bin/bash
# Resuming the context search after the machine ran out of memory at
# 23:34 on 12 September: 305 kept run directories in /tmp, a tmpfs, held
# 20 GB of state images beside the model. 8k and 64k had finished; 32k had
# just begun and 96k had not started. Same image, same server
# configuration (draft-mtp), same fixture; the only change is that kept
# runs now go to disk (TMPDIR), never RAM.
cd /home/michael/Agentware || exit 1
R=docs/results/2026-09-12
export TMPDIR=$HOME/llm/runs/2026-09-13
mkdir -p $TMPDIR

until curl -sf http://127.0.0.1:8080/health > /dev/null; do sleep 5; done
sleep 5
free -g | head -2

for pad in 32768 98304; do
  tools/tasksuite.py --pad $pad --keep \
    --json $R/moe-history-plausible-$pad.json > $R/moe-history-plausible-$pad.log 2>&1
done
echo SEARCH-DONE
