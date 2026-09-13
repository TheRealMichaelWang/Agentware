#!/bin/bash
# The follow-up to the padded-history campaign, queued behind the decode
# measurements because it needs a repack of the system image, and the image
# must not be rewritten while guests are booting from it.
#
# Why it exists: under 32k of padded history the model answered the short
# tasks in one exchange with no tool call and a report of work it had not
# done. The padding was a history of exactly such reports, so two things are
# measured apart here: the lower levels the plan named (8k, 16k) in the
# `actions` style, to find where it starts; and the `answers` style, no
# action reports in it, at 16k and 32k, to tell a length effect from a
# content effect.
cd /home/michael/Agentware || exit 1
R=docs/results/2026-09-12

until grep -q DECODE-DONE $R/decode-campaign.log 2>/dev/null; do sleep 30; done
# The pad-style argument is new in the agent; the image has to carry it.
make pack > $HOME/llm/logs/pack-followup-2026-09-12.log 2>&1 || { echo PACK-FAILED; exit 1; }

run() {
  tools/tasksuite.py --append "agentware.pad-history=$1 agentware.pad-style=$2" --keep \
    --json $R/moe-pad-$1-$2.json > $R/moe-pad-$1-$2.log 2>&1
}
run 8192 actions
run 16384 actions
run 16384 answers
run 32768 answers
echo FOLLOWUP-DONE
