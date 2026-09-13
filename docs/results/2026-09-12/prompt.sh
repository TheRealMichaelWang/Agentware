#!/bin/bash
# The prompt experiment, queued behind everything else because it repacks
# the image. What the padded-history campaign found: under a history of the
# agent's own action reports the model starts answering a new request with a
# report of work it has not done, and under the same length of history with
# no action reports in it (16k, answers style: 17/18 against 12/18) it does
# not. So the falloff is the history's content, not its length, and the
# system prompt gained one bullet saying that every request is new work and
# a report of actions not performed this turn is a false report. Measured
# three ways: no padding (the bullet must not cost anything), 32k of action
# reports (9/18 without it), and 64k (6/18 without it).
cd /home/michael/Agentware || exit 1
R=docs/results/2026-09-12

until grep -q DECODE2-DONE $R/decode2-campaign.log 2>/dev/null; do sleep 30; done
make pack > $HOME/llm/logs/pack-prompt-2026-09-12.log 2>&1 || { echo PACK-FAILED; exit 1; }

tools/tasksuite.py --keep --json $R/moe-prompt-nopad.json > $R/moe-prompt-nopad.log 2>&1
for pad in 32768 65536; do
  tools/tasksuite.py --append "agentware.pad-history=$pad agentware.pad-style=actions" --keep \
    --json $R/moe-prompt-pad-$pad.json > $R/moe-prompt-pad-$pad.log 2>&1
done
echo PROMPT-DONE
