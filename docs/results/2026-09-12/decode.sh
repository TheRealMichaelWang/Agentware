#!/bin/bash
# Phase 4 decode measurements, 12 September 2026: the server's tool-call
# grammar (lazy, from token one, absent) and every speculation mode llama.cpp
# offers that needs no second model, each measured on the same greedy
# exchanges. Runs after the suite campaign, because each mode is a server
# restart and a restart under a suite run would be a run measured against
# nothing.
cd /home/michael/Agentware || exit 1
R=docs/results/2026-09-12
MODEL=$HOME/llm/models/Qwen3.6-35B-A3B-UD-Q5_K_XL.gguf
SERVER=$HOME/llm/llama.cpp/build-vulkan/bin/llama-server

until grep -q CAMPAIGN-DONE $R/campaign.log 2>/dev/null; do sleep 30; done

start() {
  pkill -x llama-server; sleep 3
  $SERVER -m $MODEL -ngl 99 -c 131072 -fa on --jinja --spec-type "$1" \
    --host 127.0.0.1 --port 8080 > $HOME/llm/logs/server-decode-$2.log 2>&1 &
  until curl -sf http://127.0.0.1:8080/health > /dev/null; do sleep 2; done
  sleep 3
}

for spec in draft-mtp none ngram-simple ngram-mod draft-mtp,ngram-simple; do
  label=${spec//,/+}
  start "$spec" "$label"
  tools/modelbench.py --greedy --only exchange --only batching --only grammar --only repeat \
    --json $R/decode-$label.json > $R/decode-$label.log 2>&1
done

# Leave the machine on the configuration CLAUDE.md documents.
start draft-mtp restored
echo DECODE-DONE
