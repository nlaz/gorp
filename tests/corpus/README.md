# Fixture corpus

A frozen tree used by the Rust test suite and by `tools/snapshot.sh`. It stands
in for a small polyglot service repo: code in four languages, operational prose,
and two deliberately off-topic documents that act as controls (a lexical engine
should never surface them for a code query; a semantic engine should surface
them for a prose query).

**Do not edit these files to make a test pass.** Ranked output over this tree is
recorded in `tools/snapshot.txt` and diffed on every refactor phase, so any edit
here reads as a behavior change. Add a new file instead.
