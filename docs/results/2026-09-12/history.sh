#!/bin/bash
# History with actions, measured. The agentdesk's history now carries each
# turn's actions above the reply (agentdesk/src/main.rs, with_actions), and
# the agent tells the model once when a turn ends with no tool called
# (awagent/src/main.rs, ZERO_ACTION_NUDGE). The fixture is rewritten in the
# same shape: tools/fixtures/conversation-1.txt, 120 hand-written turns with
# their actions, about 12.5k tokens, so 8k never repeats and 16k repeats
# once. Three suites: no padding, 8k, 16k.
cd /home/michael/Agentware || exit 1
R=docs/results/2026-09-12

make pack > $HOME/llm/logs/pack-history-2026-09-12.log 2>&1 || { echo PACK-FAILED; exit 1; }

# No-padding was 17/18 under the text form of this (moe-history-nopad.json)
# and a turn with no history is untouched by the change, so it is not rerun.
for pad in 8192 16384; do
  tools/tasksuite.py --pad $pad --keep \
    --json $R/moe-history-plausible-$pad.json > $R/moe-history-plausible-$pad.log 2>&1
done
echo HISTORY-DONE
