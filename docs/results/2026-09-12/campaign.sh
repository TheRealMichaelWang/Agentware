#!/bin/bash
# The 12 September 2026 campaign, exactly as it was run, so the JSON beside it
# can be read against the command that made it.
#
# The server for all of it: Qwen3.6-35B-A3B-UD-Q5_K_XL on llama.cpp (Vulkan),
# 128k context, f16 KV, MTP draft, started as CLAUDE.md says. One server for
# every run, never restarted between them, so the prompt cache carries the
# system prompt (and the padding) from one run to the next.
#
# The first suite run of the day was meant to be thinking on and measured
# thinking off, because the setting was truncated on its way into the image
# (see the gotcha in CLAUDE.md). It is kept as the same-day baseline.
cd /home/michael/Agentware || exit 1
R=docs/results/2026-09-12

# Let the baseline finish; it was started by hand before this script existed.
while kill -0 44127 2>/dev/null; do sleep 10; done
mv $R/moe-thinking-on.json $R/moe-thinking-off-baseline.json
mv $R/moe-thinking-on.log $R/moe-thinking-off-baseline.log

# Phase 3: thinking on versus off.
tools/tasksuite.py --setting agent/local-thinking=on --keep \
  --json $R/moe-thinking-on.json > $R/moe-thinking-on.log 2>&1

# Phase 3: where quality falls off with context. Thinking off, the
# configuration the machine ships with; the history padded to three sizes.
for pad in 32768 65536 98304; do
  tools/tasksuite.py --append agentware.pad-history=$pad --keep \
    --json $R/moe-pad-$pad.json > $R/moe-pad-$pad.log 2>&1
done
echo CAMPAIGN-DONE
