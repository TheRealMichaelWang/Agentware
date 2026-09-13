#!/bin/bash
# The zero-action nudge (awagent/src/main.rs, ZERO_ACTION_NUDGE), measured,
# and the plausible-history curve continued with it in place.
#
# Under a realistic 8k history the model's dominant failure was to end a
# turn in one exchange with a report of work it had not done, copied in
# shape from the nearest earlier turn: 0 of 9 short-task runs passed. The
# harness now tells it, once, that nothing was done and asks again. So the
# curve's remaining levels are measured with the nudge, which is the harness
# that would ship, rather than three more measurements of the same floor.
#
# Waits for the 8k plausible suite (started by plausible.sh, whose later
# levels were cancelled) to finish on its own, then repacks the image.
cd /home/michael/Agentware || exit 1
R=docs/results/2026-09-12

while kill -0 160472 2>/dev/null; do sleep 10; done
make pack > $HOME/llm/logs/pack-nudge-2026-09-12.log 2>&1 || { echo PACK-FAILED; exit 1; }

tools/tasksuite.py --keep --json $R/moe-nudge-nopad.json > $R/moe-nudge-nopad.log 2>&1
for pad in 8192 16384 32768; do
  tools/tasksuite.py --pad $pad --keep \
    --json $R/moe-nudge-plausible-$pad.json > $R/moe-nudge-plausible-$pad.log 2>&1
done
echo NUDGE-DONE
