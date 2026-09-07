# Agentware: Thinking and Hearing On Device

This is a design, not a description. Nothing here is built yet. It covers moving
the model onto the machine's own hardware, letting a person speak to the machine
instead of typing, and making both fast enough that neither is the reason
anybody is waiting.

ARCHITECTURE.md says what the processes are. INTERACTIONS.md says what they say
to each other. This says what is being added to both, and why each piece is
where it is.

## What we are trying to be able to do

**Run with no network at all.** The model is a service on this machine, reading
weights from this machine's disk. An Agentware with no route to the outside is
an Agentware that still works. The Anthropic backend stays, because a person
with an API key and a hard question should be able to reach a larger model, but
it stops being the thing the system needs in order to function.

**Let the machine's owner choose the model.** Which model answers is already a
property of a conversation, chosen per agentdesk in the pane and offered again
in the start menu for a conversation that does not exist yet. That stays true;
what changes is that the list stops being a table compiled into the binary and
becomes the models actually installed on the machine.

**Speak as well as typing, never instead of it.** Voice adds one way in and
takes none away. Every box that can be typed into is still typed into the same
way, by the same person, through the same text box, whether or not a microphone
exists or works. Voice is allowed to be a convenience precisely because nothing
depends on it.

**Hear the reply, when the human wants it.** Per agentdesk, off by default. A
machine that talks at you unbidden is worse than one that does not.

**Run on real hardware.** Today the machine is a custom monitor QEMU invents and
a virtio disk. The design has to survive amdgpu, NVMe, USB input and a UEFI boot
without changing what any of the processes above believe about the world.

## What fast means, precisely

Vague performance goals produce vague systems. These are the numbers to design
against, measured from the moment the human commits an instruction, by pressing
Enter or by finishing a sentence:

* **The first action reaches the screen within a second.** Everything between
  the human committing and the cursor beginning to move is overhead, and a
  second is the budget for all of it.
* **Actions after the first are paced by the cursor, not by the model.** The
  cursor takes time to travel because a human has to be able to follow it. That
  is a deliberate cost and it should be the *only* one: the model should have
  the next action decided before the current one has finished being performed.
* **The reply begins within half a second of the last action.**
* **Nothing above depends on the network.**

The single sentence that governs the whole design: **the model should never be
the reason the human is waiting.** Today it always is, by a wide margin. The
target is a system where the visible pacing comes from deliberate animation and
everything else hides behind it.

There is a corollary worth stating because it will be uncomfortable later. Once
inference is local and warm, `FLIGHT` and `KEYSTROKE` in the compositor become
the entire visible budget. Those constants were chosen when they were noise
against a two second API call. They will not be noise against a two hundred
millisecond one, and they will have to be revisited on their own terms rather
than left where an earlier bottleneck happened to make them invisible.

## What is added

Two new services in the Supervisor's service table, both long lived, both
started at boot, both reached the way everything else is reached: over
descriptors PID 1 creates and hands over at fork.

```
PID 1 supervisor
├── haimanager        compositor: DRM, input, AWML, agent surface
├── awinference       NEW. the model: weights, caches, grammars
├── awvoice           NEW. the ear and the voice: audio, VAD, ASR, TTS
├── agentdesk × N     conversation, transcript, draft
│   └── awagent       per turn: the loop
└── apps × M
```

Three new wires:

```
awagent    <-> awinference     one exchange of a conversation
agentdesk  <-> awinference     warming, and nothing else
awvoice    ->  haimanager      words, to be typed where focus is
```

And one new place on disk: `/models/`, on the state volume.

## awinference

**A model behind a socket, and nothing more.** It does not know what a workspace
is, has never heard of an application, and cannot tell an agent from an
agentdesk. It is given a conversation and gives back the model's answer to it.
Everything that makes Agentware what it is lives on the other side of this
socket and stays there.

It owns three things that must not be built twice per turn:

* **The weights.** Loaded once, resident until the machine chooses a different
  model. A model that must be read off disk per turn is a model that cannot meet
  any of the numbers above.
* **The KV caches**, one per agentdesk, described at length below because their
  management is most of what this service is for.
* **The grammars.** The action vocabulary is closed, so the shape of a tool call
  is known in advance and can be compiled once.

## One model, chosen for the machine

A model is fifteen to twenty gigabytes. Two of them leave nothing for the caches,
and the caches are where the latency actually lives. So **one model is resident
at a time**, named in `settings.xml`, and it is a property of the machine rather
than of a conversation.

That is a deliberate exception to the settled decision that the model is the
conversation's, and it is worth being honest about why: the decision was about
preference and this is about a resource. The rule still holds for the hosted
backends, where nothing is resident and any conversation may name any model.

**The hosted backends do not go away.** The selector in the pane and in the start
menu keeps every Claude configuration it has today and gains exactly one entry,
called **Local**. Which model that is comes from Settings, and a desk that
selected it is selecting "whatever this machine runs locally" rather than a
particular model. Naming the model in the dropdown instead would imply a
specificity the selection does not have: change the setting and the desk has not
chosen anything different, it has simply been answered by something else.

What the model *is* still belongs on screen, just not in the control that chooses
it. The pane's working line already names the backend while a turn runs, and that
is the honest place for it: the selection is Local, and the line says which model
is doing the work.

If no model is installed the entry is not offered, in the same way an application
without a `description.txt` is not offered in the start menu.

**The local entry is the default.** A machine with no network and no API key
should work, and today it does not. That inversion is the point of this whole
document.

Two consequences of a mixed machine, where some agentdesks are Local and others
are Claude. **A desk on a hosted backend needs no slot at all**, so the cache
budget below is divided among local desks only, and a machine where most work is
hosted has more room for the ones that are not. And **changing a desk's selector
to Local is a prefetch trigger**, the same as switching to it: the change applies
from the next turn, so the conversation can begin loading the moment the dropdown
closes rather than when the human presses Enter.

Changing the model in Settings evicts the old one and loads the new. That is
four to eight seconds of sequential read plus a prefill, during which the machine
cannot answer, so it needs two things: a visible state on the Settings page, and
a rule for turns in flight. **A running turn is the human's work and a model
change is a preference, so the swap waits.**

## Caches: resident, saved, gone

A conversation's KV cache is three to ten gigabytes depending on the model and
the length, and rebuilding one from nothing is not a small cost. Prefill is
compute-bound, and re-prefilling a long conversation is tens of seconds. **The
whole memory design exists to avoid ever paying that twice.**

There are three states a conversation's cache can be in:

* **Resident.** The slot is in memory. A turn starts immediately.
* **Saved.** The KV has been written to disk. Coming back is a sequential read
  of a few gigabytes, a second or two, rather than a rebuild.
* **Gone.** Nothing is kept and the next turn prefills from scratch.

A **slot is an agentdesk**, which is the mapping llama.cpp already has. Eviction
from resident to saved is least-recently-used, with two things pinned: **the
agentdesk on screen is always resident**, and a desk with a turn running keeps
its slot until the turn ends. If every slot is pinned, a desk being switched to
waits rather than interrupting somebody's turn.

Saved caches get a disk budget, on the order of two hundred gigabytes, and age
out of it least-recently-used as well. Past that they are gone, which is a
recoverable state rather than a lost one.

**Closing an agentdesk deletes its cache outright**, from whichever state it is
in. The conversation has ceased to exist, so the cache is not old, it is garbage.

### The cache is an optimization and must always yield

Everything above is a speed-up, and nothing on this machine may depend on it.
**At least ten gigabytes stays available for the operating system, the
compositor, the applications and everything else that is not a model.** That is
a floor, not a target, and it is the first number in the budget rather than
whatever is left over.

So the memory arithmetic runs one way only:

```
  cache budget = total memory - the floor - the resident model
  slot count   = cache budget / bytes per slot
```

Both of those are decided at startup, and the elasticity lives entirely in the
disk tier. That matters because llama.cpp reserves KV for its slots up front
rather than growing them on demand, so a design that expected to hand memory back
under pressure would be expecting something the runtime does not do. Sizing the
slots so that the *worst* case still clears the floor is what makes the floor a
guarantee instead of a hope. Fewer resident conversations and more saved ones is
the correct answer to a machine with other work to do.

**A conversation that outgrows its slot is not a memory problem.** It is a
context problem, and it is answered by trimming history at the turn boundary
rather than by finding it more room. There is no case where a long conversation
takes memory away from the floor, because a slot's size is fixed and a
conversation is bounded to fit it.

Which gives a number worth setting once and deriving everything else from: **the
slot size, the context budget and the point where the model's quality falls off
should all be the same figure.** Measurement so far puts that near 32k. Sizing
slots larger buys context the model cannot use well; sizing them smaller wastes
nothing but forces trimming sooner.

### Why this rather than letting the kernel page it out

The obvious objection is that this is virtual memory, badly reimplemented. It is
not, for three reasons, and the first is decisive on its own.

**GPU memory is not swappable.** The KV cache lives in a buffer the integrated
GPU reads, which is an amdgpu allocation and not ordinary anonymous pages. The
kernel's swapper has no jurisdiction over it. What exists instead is TTM buffer
eviction inside the driver, which fires at allocation time on the driver's
schedule, and which would reintroduce exactly the class of bug this project spent
two rounds of measurement removing: multi-second stalls with no attributable
cause.

**Attention is the worst possible access pattern for paging.** Swap works when
access is localized. Attention reads the entire cache for every generated token,
so a cache with any part paged out faults the whole thing back in per token. And
the granularity is wrong in the same direction: swap evicts four-kilobyte pages,
while the useful unit is one whole conversation. Half a KV cache resident is
worth nothing.

**The knowledge is here and not there.** The kernel sees anonymous pages. This
service can be told which desk is on screen, which desk was just closed, and
which desk somebody is composing in right now. The last of those is not eviction
policy at all, it is prefetch, and no general pager can do it because the signal
is that a human started typing.

What the kernel *should* keep is the weights, which llama.cpp mmaps from the
GGUF: file-backed, read-only, shared, and cheap to re-read. The division is
clean. **File-backed and clean belongs to the page cache; device-resident and
dirty belongs here.**

### Prefetch on switch, not on send

The cache is brought in **the moment the human switches to an agentdesk**, not
when they send a prompt.

Switching is the earliest reliable signal of intent, and it buys the entire
window in which somebody reads the transcript, thinks, and composes. That window
is seconds, which covers a restore from disk comfortably and can cover a prefill.
By the time a prompt is committed the conversation is warm, and the visible cost
of switching desks is nothing.

This is what makes the whole tiering work. Without it, "saved" would mean a
second or two of delay on the first prompt after every switch. With it, that
delay happens while somebody is still reading, which is to say it does not
happen.

It composes with voice for free. The desk is switched to, the cache begins
loading, the human says "Agentdesk, ...", `awvoice` transcribes while the load is
still in flight, and the words arrive in a composer whose conversation is already
resident.

Two details it needs. A switch away before the load finishes should **cancel**
it, or rapidly cycling workspaces with F1 would queue a load per desk passed
through. And the load is asynchronous: the agentdesk asks and carries on
rendering, because a workspace that stalls while its cache arrives is a worse
outcome than the delay it was avoiding.

### Caches do not outlive a reboot

Conversations die with their workspace, by explicit decision. A saved cache that
survived a boot would therefore be caching something that no longer exists.
The files live on the machine's own storage in a directory the supervisor clears
at startup, and the rule is worth stating plainly because it is the kind of thing
that gets accidentally made persistent later: **a cache follows the lifetime of
the thing it caches.**

This does mean the state volume stops being the 64MB image it is today. Models
are tens of gigabytes and saved caches are budgeted at two hundred, so on real
hardware this is a partition rather than a file, and it is worth deciding early
whether settings, models and caches share it or are separated by lifetime.

**Why it is our process and not just llama.cpp's server.** The first version
should nonetheless *be* llama-server, because that costs nothing and proves the
model before any of the below is worth arguing about. `Backend` is one trait
with one method, so replacing it later is a swap rather than a rewrite. What
follows is why the swap is eventually worth making, honestly ranked, because the
weakest of these reasons is the one most often given first.

* **No HTTP, no JSON, no SSE on the latency path.** Every other wire in this
  system is length-prefixed frames over a unix socket. Talking to llama-server
  means encoding a growing conversation as JSON and parsing server-sent events a
  token at a time, on every exchange, in the one place the whole design is
  trying to make fast. This is the strongest reason and it is measurable.
* **Cache slot policy per agentdesk.** An agentdesk warms the cache on a draft;
  the turn that follows must land on the slot that warming filled or the work is
  wasted. Which conversation owns which slot is knowledge only Agentware has,
  and delegating it means hoping a general purpose server's prefix matching
  happens to do the right thing. This reason does not exist until draft warming
  does.
* **More than one model at a time.** Which model answers is a property of a
  conversation, so two agentdesks may reasonably want two different ones. One
  llama-server holds one model; serving that from outside means running two
  servers and deciding between them somewhere.
* **The grammar is fixed and could be compiled once** rather than per request.
* **Service table citizenship.** Restart policy, readiness gating, descriptor
  handoff and clean shutdown are things PID 1 does for its own; a third-party
  HTTP server is an awkward guest.

And one reason that is not a performance argument and does not need to pretend
to be: this is a userland where every process speaks one protocol and the
reasoning is written down next to the code. A C++ HTTP server in the middle of
it is a wart. Design coherence has been chosen over expedience everywhere else
in this system, and it is a legitimate thing to choose here.

**The stack.** llama.cpp, for reasons that are specific rather than
sentimental. Its Vulkan backend works on RDNA3.5 without needing ROCm to be
perfect on an integrated GPU. GBNF grammars are built in, which is the
constrained decoding this system's closed vocabulary makes unusually valuable.
Slot based KV reuse is exactly the property the turn structure is built to
exploit. Speculative decoding and prompt lookup are both supported. GGUF is
where quantized mixture of experts models actually live. And there is no Python
runtime, which matters in a musl userland.

## awvoice

**Always listening, rarely transcribing, addressed by name.**

The service runs from boot and owns the audio device. Voice activity detection
runs continuously and costs almost nothing. A wake word model runs on whatever
the detector marks as speech, and that is the whole of what happens until the
machine is addressed. **Nothing is transcribed until a wake word fires**, and no
audio ever leaves the machine, because the recognizer is on it. That is a
property worth having deliberately rather than as a side effect.

**Two names, two destinations.** The wake word is not a trigger, it is an
address. It says which of two places the words are going, and they are the two
places a person already puts a prompt:

* **"Agentware, ..."** goes where the start menu's prompt goes: a **new
  agentdesk**, beginning with what was said.
* **"Agentdesk, ..."** goes where the pane's composer goes: the **agentdesk on
  screen right now**, as the next message in that conversation.

Nothing else is addressable. Voice does not type into an application's fields,
does not click anything, and has no way to reach the agent surface. It puts
prompts in the two boxes prompts go in, which is the smallest thing that is
worth having and the largest thing that can be built without inventing a second
way to drive the machine.

**The words are typed, visibly, before they are sent.** `awvoice` hands the
compositor a destination and the text as it stabilizes; the compositor puts it
in the destination's box the way a keystroke would, and submits when the speech
ends. For "Agentdesk" that means going through the same `Client::act` a human's
typing into the composer goes through, so there is still exactly one function
where input becomes an event. For "Agentware" it means opening the start menu
and filling its prompt, which is chrome the compositor already owns.

Typing rather than submitting silently is not decoration. It is how a person
sees what was heard before it is acted on, and it is what makes Escape mean
what it already means.

**The other reason it is typed:** the destination fills up as a *draft*, and a
draft is what the agentdesk warms the model on. Speech gets the same head start
typing gets, from the same mechanism, with nothing written for it.

The "Agentware" path has almost nothing to warm, which is convenient. A new
conversation is a fresh slot holding the system prompt, the tools, and one
sentence, so the only cost is the one prefill every agentdesk pays once.

**A rejected alternative, recorded so it is not rediscovered.** `awvoice` could
create a `uinput` device and inject key events, making voice literally a
keyboard with no new protocol at all. What kills it is that a wake word has to
choose a destination, and a keyboard cannot choose one: it types where focus is.
Routing is the entire feature, and routing is not something a key event can
express.

**Speaking back** is the reverse and stays optional, off by default, per
agentdesk. The agentdesk owns the conversation, so it decides what is worth
saying aloud and hands it over. One direction, one wire.

This does reach back into `awagent`. Today `Delta::Text` is deliberately
swallowed as it streams, because whether text is narration or the reply is only
known once the exchange completes and the pane must not show the reply twice.
That is right for a pane and wrong for a voice, where beginning early is the
entire point. Something has to give, and the cheapest answer is probably to
speak the narration too, since "I am opening the spreadsheet now" is exactly
what a person would want to hear.

## What changes in what already exists

**awagent** gains a second `Backend` implementation speaking the OpenAI
compatible shape, which is the one shape that reaches llama.cpp, mistral.rs,
vLLM and most hosted providers at once. The Anthropic backend stays as it is.
The harness itself barely changes, which is the point of having put the model
behind a trait whose whole contract is one streamed exchange.

**agentdesk** gains one job: warming. While a human composes, and only while no
turn is running, the desk sends its conversation and the draft to `awinference`
so that the cache is hot before Enter is pressed. It already holds the draft in
`composing`, so this is not new state, only a new use of it.

This is deliberately not a voice feature. Voice and typing both produce a
growing draft, so both get the same speedup, and voice inherits it rather than
having its own path.

**haimanager** accepts words from `awvoice` and routes them to focus. It also
has to face the animation constants once the model stops hiding them.

**supervisor** gains two service table entries and the descriptors for three new
wires. It sees no more than it sees today: not a prompt, not a conversation, not
a word of transcribed speech. It creates sockets and steps out of the way.

**awproto** gains the inference wire, and `turn::BACKENDS` stops being a
compile-time table.

## Models are packages

An installed model is a folder under `/models/`, holding the weights and a
little metadata, in the same spirit as `/apps/<name>/`: adding one is dropping a
folder in place, exactly as adding a theme is dropping an XML file. Nothing is
registered anywhere and no code lists what exists.

They live on the **state volume**, not in the system image. A model is tens of
gigabytes and belongs to the machine, not to the install. `make pack` has no
business rewriting it, for the same reason it stopped having any business
rewriting `/home`.

## The optimizations, and why each one is worth doing

Ordered by how much they are expected to matter, which is not the order they
will be done in.

### What is actually measured, and what it changes

One real data point exists and it should govern this list rather than the
arithmetic. A dense 27B runs healthily on this machine, with a context window of
128k, but **its quality degrades noticeably past roughly 32k tokens**. No
speculative decoding was involved.

That is a different constraint from the one the arithmetic predicts. It says
throughput is survivable and **context is the scarce resource.** Which means the
first question about any optimization below is not "does it generate faster" but
"what does it do to the token budget".

The budget, roughly, for a turn on this machine:

```
  system prompt + tool definitions      ~3k
  conversation history                  grows without bound, never trimmed
  one application view                  1k to 2k
  a ten-action turn's re-read views     10k to 20k
```

A busy turn in a long-running agentdesk reaches 32k without doing anything
unusual. That is the thing to design against.

It also means the original instinct about the system prompt being long was
right, for a reason other than the one given. It costs little in time: it is
prefilled once per slot and then sits at the head of that slot's cache. It costs
**eight percent of the usable context**, permanently, in every turn, in every
agentdesk.

That is worth correcting carefully, because an earlier draft of this document
said the system prompt is prefilled once for the life of the machine. It is not.
Each slot has its own KV cache, so the prefill is once per agentdesk rather than
once per turn, which is still the right shape of saving but a smaller one than
was claimed.

### Spend the context budget deliberately, and cut in the right place

This section has been wrong twice in opposite directions, so both errors are
recorded rather than quietly fixed.

The first draft said **never prune**, because removing anything from the middle
of a prompt invalidates every cached token after it. That weighed a cache miss
against nothing.

The second said **prune superseded application views mid-turn** and take the
re-prefill, on the grounds that a cache miss is cheap. It is not. Prefill is
compute-bound, and re-prefilling from position 10k of a 25k context is fifteen
thousand tokens, which on this machine is tens of seconds. That is far worse
than the problem it was solving.

The mistake both times was treating "context" as one thing. It is two, with
completely different shapes:

**Views grow within a turn and reset at the turn boundary.** Ten actions might
add fifteen thousand tokens, and then the turn ends and every one of them is
discarded, because the turn's tool detritus never enters `history`. This growth
is bounded by the length of one turn and it corrects itself. It is not the
problem, and cutting into it costs a re-prefill of everything after the cut.

**History grows for the life of the agentdesk and is never trimmed.** It is also
the stable prefix that everything else caches behind, which is exactly why it
feels safe to leave alone and exactly why it eventually is not.

So: **leave a turn's context alone, and bound history at the turn boundary.**
The boundary is where a prompt is being assembled fresh anyway, where a
divergence is already going to be paid for, and where the human is about to wait
for a model call regardless. Summarizing older exchanges into a single message is
the usual mechanism and it belongs in the agentdesk, which owns the conversation
and is the only process that can decide what was important about it.

The thing to do about views is not to cut them but to **not produce them**: keep
the agent view small, which the spreadsheet work already showed is worth a great
deal, and reconsider whether every action needs a full re-read attached or only
those where something material changed.

### Choose a sparse model, not a small one

The largest single decision, and the only one that cannot be recovered from by
being clever elsewhere. Generation speed is memory bandwidth divided by bytes
read per token, and this machine has roughly 256 GB/s. A dense 30B at four bits
reads about 17GB per token and is therefore capped near 15 tokens a second
before reality intervenes.

A mixture of experts model reads only its active experts. Something in the 30B
total, 3B active class reads about 2GB per token, which is the same quality tier
for roughly an eighth of the bandwidth. And the property that makes this machine
mediocre for dense models, memory that is plentiful but slow, is exactly what
makes it good for sparse ones: 64GB of unified memory holds a large total model
that would not fit on a discrete consumer card, while only the active slice is
ever read.

### Constrain generation to a grammar

The action vocabulary is closed and derived rather than declared, so the shape of
a valid tool call is known before the model speaks. Constraining generation to it
makes a small model reliable at this job, which is the usual reason people do it.

The reason that matters more here: **when the grammar leaves one legal token, the
model does not have to be run at all.** A tool call against a fixed schema is
largely structural tokens, every one of them forced, and each token skipped is an
entire read of the model that does not happen. On hardware where reading the
model *is* the cost, that is not a small saving.

### Speculate, using what the output already is

Almost every semantic token in a tool call is a string copied out of the model's
own context. The application name came from `list_apps`. The action is one of
fifteen from a list the compositor derived and put in the view. The target id
appeared verbatim in the markup a few hundred tokens earlier. This system asks
the model to select from what it has just read, and selection from context is the
most predictable output there is.

That means prompt lookup speculation works without a draft model at all: search
the prompt for the current suffix and propose what followed it. If a draft model
earns its place later, the NPU is where it should live, because it runs during
generation, which is exactly when speech recognition is not.

The honest limit: none of this helps thinking tokens, which are free-form prose,
unconstrained and unpredictable. Which leads to the next item.

### Make thinking a budget rather than a default

Adaptive thinking is on for every non-Haiku model today, and thinking is probably
the majority of the tokens a turn generates. It is also the part that speculates
worst and the part a grammar cannot touch.

This design deliberately removed most of what a model would otherwise have to
reason through: the legal actions are listed, the ids are given, the vocabulary
is closed, and a rejection explains itself. How much thinking that leaves worth
paying for is an empirical question nobody has asked yet, and it should be a
setting rather than a constant.

### Keep the cache warm, and keep the prompt append-only

Within a turn, every exchange is the previous one plus an assistant message and a
tool result. That is a perfect prefix chain, ten deep in a busy turn, seconds
apart. At the turn boundary the tool detritus is discarded and the next turn
starts from a small clean prompt.

That is already the right structure and it arrived by accident of decisions made
for other reasons: append-only where caching pays, pruned where it does not.

It should be left alone. The one cut this design makes is to history at the turn
boundary, which is where the prompt is reassembled anyway and where the cost is
already being paid.

Sparse models help here too, and not only at generation time: every cache
invalidation is a prefill, and prefill on a model with three billion active
parameters is a fraction of prefill on a dense twenty-seven billion. The cost of
being wrong about any of this is smaller with an MoE.

KV cache quantization is worth trying for the same reason. At 32k and above the
cache is large enough that holding it at eight or four bits changes both the
memory it occupies and the bandwidth every token's attention costs.

### Warm the draft while the human is still composing

Everything in the prompt except the last message is knowable before the human
finishes writing it. Prefilling it during composition costs nothing, because
composition is precisely when no turn is running and the model is idle.

### Speculatively perform the reads, never the intents

The agent wire splits cleanly into `query`, which is idempotent and has no effect
on anything, and `intent`, which changes what is on a human's screen. That split
was drawn for other reasons and it happens to be exactly the line speculation can
safely cross.

While a draft is still being composed, the read phase can be run ahead: what
applications are open, what the likely one looks like. Being wrong costs a socket
round trip and some cache. **Being wrong about an intent would put something on
the human's screen that they did not ask for, so intents are never speculative.**
This is a new settled decision and it should be treated as one.

### Say more in fewer tokens

A JSON tool call carries about eight tokens of information in twenty five tokens
of syntax. With a local model and a grammar there is no obligation to the
provider's conventions, and a shape closer to the system's own wire would cut
generated tokens substantially. The risk is real, since models are trained on
JSON tool calling, and this is worth measuring rather than assuming.

### The harness fixes that pay off either way

Independent of everything above, and worth doing before any of it: reuse the
HTTPS connection rather than handshaking per exchange, put a cache breakpoint on
the conversation rather than only the system prompt, delete the `read_app`
description that tells the model to do what the system prompt tells it not to,
say that tool calls may be batched, and poll immediately in `open_app` instead of
sleeping first.

### The compositor, once it is the bottleneck

`FLIGHT` is 600ms regardless of distance, so a cursor already on its target
spends 600ms travelling nowhere. Making it proportional costs nothing in
legibility. `KEYSTROKE` is 45ms per character, which is already faster than any
human types but still three seconds for a sentence; a person reads "it is
typing" in the first fraction of a second and does not need the rest to confirm
it, so the rate should ramp. And the prefill of the next exchange can overlap the
animation of the current action, which makes the legibility free rather than
additive.

## What must not change

Every one of these is a settled decision that this work is capable of breaking
by accident, which is why they are listed rather than assumed.

* **Intents are not events, and one function serves both.** Voice does not get a
  path to applications. Speculation does not get a path to intents.
* **The Supervisor sees no content.** Two more services and three more wires, and
  it still creates sockets and steps out of the way.
* **Identity is a capability, not a claim.** Scoping stays a property of which
  connection a request arrived on.
* **The agent owns nothing durable and is never restarted.** Making inference
  resident does not make the agent resident. The expensive warm state moves into
  a service precisely so the per-turn process can stay disposable.
* **Applications send whole trees, and the model receives whole state.** Nothing
  here introduces a diff in either direction.

## Bare metal

The target is known and it is the machine this is being written on: a Framework
Desktop with an AMD Ryzen AI MAX+ 395 and a Radeon 8060S, 61GB usable, running
Ubuntu on 7.0.0-generic. That means the port is not an investigation. Every
question about what the hardware needs can be answered by reading what Ubuntu is
already doing with it.

What is present and bound on the running system:

* **`amdgpu`** on the iGPU at `c3:00.0`.
* **`amdxdna`** on the NPU at `c4:00.1`. The driver works on this kernel today,
  which settles whether the NPU is reachable at all. What runs on it is a
  separate question.
* **`snd_hda_intel`**, with an ALC623 analog codec exposing a capture device.
  This is the good news of the whole survey: audio here is plain HD Audio, not
  the ACP and SOF path that would have made the microphone a project of its own.

Every kernel symbol the port needs is already in Ubuntu's config:
`DRM_AMDGPU`, `BLK_DEV_NVME`, `HID_GENERIC`, `I2C_HID`, `USB_XHCI_HCD`,
`SND_HDA_INTEL`, `DRM_ACCEL_AMDXDNA`, `EFI_STUB`. All as modules except the
last two.

**That last detail is the one piece of real design work.** The initramfs holds
the supervisor and `/dev/console` and nothing else, on purpose: PID 1's pages
live in RAM so that a missing or dying disk is something it reports rather than
something that takes it down. There is no module loader, so everything needed to
bring up a screen has to be built in, and amdgpu built in needs its firmware
before any filesystem is mounted.

The firmware for this ASIC is small. The `gc_11_5`, `psp_14`, `smu_14`,
`dcn_3_5`, `vcn_4_0_5` and `sdma_6_1` sets together are 3MB compressed across
every variant, and this machine needs one variant of each. Against an initramfs
that is currently 250KB that is nothing, and putting it there is
philosophically right: you cannot report a firmware error on a screen you could
not bring up, so the firmware belongs with the thing that has to work before the
disk does.

The rest is ordinary: NVMe instead of virtio-blk with the device named by the
`agentware.system=` argument that already exists, USB and i2c HID instead of
QEMU's tablet behind the evdev abstraction that already exists, an ESP and the
EFI stub instead of QEMU loading the kernel directly, and a DHCP client in
userland now that the kernel will not configure the interface from a boot
argument.

### A concern this document previously raised and now withdraws

An earlier draft claimed the compositor's software rasterizer would starve the
model of memory bandwidth. That is wrong by about two orders of magnitude and
the arithmetic is worth writing down so it is not raised again.

A full repaint at 2560x1440 is 14.7MB of writes. At sixty frames a second that
is 880 MB/s, call it 2 GB/s with the reads that compositing does. Against a
256 GB/s bus that is under one percent. The model, meanwhile, is reading tens of
gigabytes a second: a dense 27B at four bits reads about 14GB per token, so even
at five tokens a second it is using seventy. The compositor is not a rounding
error away from mattering, it is a rounding error.

What remains true is much narrower: software rasterization costs **CPU time**,
7 to 8ms per frame measured at 1440p, and that scales with pixels. At 4K it is
roughly double, which is still inside a 60Hz frame but no longer comfortably.
That is a compositor latency question with a known shape and a known set of
answers, and it has nothing to do with the model.

## Developing this before the hardware port

**GPU inference cannot be measured from inside QEMU.** The guest sees
virtio-gpu, which is a display device: it presents a framebuffer and sets modes,
and exposes no compute at all. There is no Vulkan compute queue and no ROCm
device in there. llama.cpp inside the guest would find nothing and fall back to
the vCPUs, which is fine for proving the plumbing works and useless for any
number.

The way through is to **run the model on the host during development and let the
guest reach it over slirp.** Ubuntu already has the iGPU working, so llama-server
on the host is a real GPU-backed model; the guest's `awagent` talks to it at
`10.0.2.2` over the NIC that is already there for the Anthropic backend.

That decouples almost everything in this document from the hardware port. The
OpenAI-compatible backend, model selection, grammars, speculative decoding,
draft warming, context pruning and every measurement of them can be built and
tuned in QEMU today, against real inference speed, with no bare metal work at
all. What it does not exercise is the service being in PID 1's table and its
descriptors being handed over at fork, which is a day's work to add afterwards.

The same trick covers voice: QEMU can present an emulated audio device, so
`awvoice`, the wake word routing and the compositor's side of it can all be
built and proven before the real ALC623 is ever touched.

## Open questions

* **Does llama.cpp build against musl with the Vulkan loader?** This is now the
  only unknown that changes the shape of the design rather than a number in it.
  If it wants glibc, either `awinference` becomes the one glibc component or it
  becomes ours sooner.
* **Do slot save and restore work with the Vulkan backend and a quantized KV
  cache, and how long do they take for a slot of a few gigabytes?** The whole
  three-tier design rests on a restore being a second or two rather than six. If
  it is six, the answer shifts toward simply holding more slots and letting the
  disk tier be a last resort.
* **Does llama.cpp reserve KV for every slot at startup, or lazily?** This
  document assumes the former, which is what makes the memory floor a guarantee
  rather than a hope. If allocation is lazier than assumed, the slot count can be
  more generous than the arithmetic here suggests.
* **Is a sparse model actually better here than the dense 27B that is known to
  work?** The arithmetic says yes by a wide margin. The one measurement that
  exists says the dense model is already survivable. Only a comparison settles
  it, and it should compare context degradation as well as tokens per second,
  because that is the constraint that turned out to bind.
* **How much thinking does a turn actually need?** One experiment, and both the
  latency and the context budget depend on the answer.
* **Is the compositor single-output?** The difference between a laptop panel and
  real display handling is not visible from inside QEMU's invented monitor.
* **What runs on the NPU, in practice?** The driver is bound and the device is
  there. Whether the toolchain will take a speech model, and later a draft model,
  is a separate question from whether the hardware is reachable.
