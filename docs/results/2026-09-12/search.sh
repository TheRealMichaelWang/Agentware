#!/bin/bash
# The context curve for the tool-call history at the sizes that matter:
# 32k, 64k, 96k, every task, three runs each. The fixture is about 22k tokens once
# its calls are rendered by the chat template, so 32k repeats it once, 64k
# three times and 96k four; each run records that. A long turn grows about
# 58k tokens over its history, so at 96k the traverse runs hit the 128k
# slot; that is the memory bound, recorded as it happens.
#
# Waits for the 8k suite (pid 190939, started by history.sh) to finish on
# its own. Same packed image.
cd /home/michael/Agentware || exit 1
R=docs/results/2026-09-12

while kill -0 190939 2>/dev/null; do sleep 10; done

# 64k first: the midpoint decides most. Then 32k, then 96k.
for pad in 65536 32768 98304; do
  tools/tasksuite.py --pad $pad --keep \
    --json $R/moe-history-plausible-$pad.json > $R/moe-history-plausible-$pad.log 2>&1
done
echo SEARCH-DONE
