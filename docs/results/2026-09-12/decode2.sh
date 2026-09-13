#!/bin/bash
# One more decode measurement, after the first round showed n-gram lookup
# beating the MTP draft head once a turn has context to copy from and losing
# to it on the first exchange: the two together. Queued behind the follow-up
# campaign because it restarts the server.
cd /home/michael/Agentware || exit 1
R=docs/results/2026-09-12
MODEL=$HOME/llm/models/Qwen3.6-35B-A3B-UD-Q5_K_XL.gguf
SERVER=$HOME/llm/llama.cpp/build-vulkan/bin/llama-server

until grep -q FOLLOWUP-DONE $R/followup-campaign.log 2>/dev/null; do sleep 30; done

start() {
  pkill -x llama-server; sleep 3
  $SERVER -m $MODEL -ngl 99 -c 131072 -fa on --jinja --spec-type "$1" \
    --host 127.0.0.1 --port 8080 > $HOME/llm/logs/server-decode-$2.log 2>&1 &
  until curl -sf http://127.0.0.1:8080/health > /dev/null; do sleep 2; done
  sleep 3
}

start draft-mtp,ngram-mod draft-mtp+ngram-mod
tools/modelbench.py --greedy --only exchange --only batching --only grammar --only repeat \
  --json $R/decode-draft-mtp+ngram-mod.json > $R/decode-draft-mtp+ngram-mod.log 2>&1

start draft-mtp restored
echo DECODE2-DONE
