#!/bin/bash
# The context curve measured properly: the history is a hand-written
# conversation of the kind a person has with this machine (tools/padding.py,
# tools/fixtures/conversation-*.txt), sized by the model's own tokenizer,
# written into each run's state image, read by the agent from a file. Four
# sizes. The fixture is about 15k tokens, so 8k and 16k never repeat a turn,
# 32k starts over once and 64k three times; the suite prints and records
# how many times, so the reader knows at which size the history stopped
# being a single conversation.
#
# Queued behind the prompt experiment because it repacks the image (the
# agent's padding hook changed from generating text to reading a file) and
# the image must not be rewritten under a booting guest.
cd /home/michael/Agentware || exit 1
R=docs/results/2026-09-12

until grep -q PROMPT-DONE $R/prompt-campaign.log 2>/dev/null; do sleep 30; done
make pack > $HOME/llm/logs/pack-plausible-2026-09-12.log 2>&1 || { echo PACK-FAILED; exit 1; }

for pad in 8192 16384 32768 65536; do
  tools/tasksuite.py --pad $pad --keep \
    --json $R/moe-plausible-$pad.json > $R/moe-plausible-$pad.log 2>&1
done
echo PLAUSIBLE-DONE
