"""Tests for run_eval's checkpointing.

A full kernel or CoSQA run takes hours — `--sort path` costs 3.7-4.4x on the
kernel and rg-oracle issues up to a dozen scans per query. Three such runs were
interrupted in one session and each threw away every scan already paid for
without producing a number.

The risk a checkpoint introduces is worse than the one it removes: silently
mixing results from two different runs produces a table that looks fine and
means nothing. Most of these tests are about refusing that.
"""

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import run_eval  # noqa: E402

HDR = dict(queries_fp="abc123", modes="bm25,rg", corpus="/c")


def ck(path):
    return run_eval.Checkpoint(path, **HDR)


# --- basic round trip --------------------------------------------------------

def test_no_path_means_no_checkpointing(tmp_path):
    c = run_eval.Checkpoint(None, **HDR)
    c.put(0, "bm25", 3)
    assert c.get(0, "bm25") is run_eval.NOT_DONE
    c.close()


def test_a_result_survives_a_restart(tmp_path):
    p = tmp_path / "ck.jsonl"
    c = ck(p)
    c.put(0, "bm25", 3)
    c.put(1, "bm25", None)
    c.close()

    c2 = ck(p)
    assert c2.get(0, "bm25") == 3
    assert c2.get(1, "bm25") is None      # a miss is a result, not absence
    assert c2.get(2, "bm25") is run_eval.NOT_DONE
    c2.close()


def test_a_miss_is_distinguished_from_unscored(tmp_path):
    # The trap: `None` means "scored, found nothing" and must not be confused
    # with "not scored yet", or a resumed run silently rescores every miss —
    # or worse, treats unscored pairs as misses and reports a lower number.
    p = tmp_path / "ck.jsonl"
    c = ck(p); c.put(0, "bm25", None); c.close()
    c2 = ck(p)
    assert c2.get(0, "bm25") is None
    assert c2.get(0, "rg") is run_eval.NOT_DONE
    c2.close()


def test_results_are_keyed_by_mode_as_well_as_query(tmp_path):
    p = tmp_path / "ck.jsonl"
    c = ck(p); c.put(0, "bm25", 1); c.put(0, "rg", 7); c.close()
    c2 = ck(p)
    assert c2.get(0, "bm25") == 1 and c2.get(0, "rg") == 7
    c2.close()


def test_each_result_is_flushed_immediately(tmp_path):
    # Buffered writes lose exactly the work the checkpoint exists to keep.
    p = tmp_path / "ck.jsonl"
    c = ck(p)
    c.put(0, "bm25", 3)
    assert len(p.read_text().splitlines()) == 2      # header + one result
    c.close()


# --- refusing to mix runs ----------------------------------------------------

def test_a_checkpoint_from_a_different_query_set_is_refused(tmp_path):
    p = tmp_path / "ck.jsonl"
    ck(p).close()
    try:
        run_eval.Checkpoint(p, queries_fp="DIFFERENT", modes="bm25,rg", corpus="/c")
    except SystemExit as e:
        assert "different run" in str(e)
    else:
        raise AssertionError("expected SystemExit")


def test_a_checkpoint_from_a_different_mode_list_is_refused(tmp_path):
    p = tmp_path / "ck.jsonl"
    ck(p).close()
    try:
        run_eval.Checkpoint(p, queries_fp="abc123", modes="bm25", corpus="/c")
    except SystemExit:
        pass
    else:
        raise AssertionError("expected SystemExit")


def test_a_checkpoint_from_a_different_corpus_is_refused(tmp_path):
    p = tmp_path / "ck.jsonl"
    ck(p).close()
    try:
        run_eval.Checkpoint(p, queries_fp="abc123", modes="bm25,rg", corpus="/other")
    except SystemExit:
        pass
    else:
        raise AssertionError("expected SystemExit")


# --- surviving a hard kill ---------------------------------------------------

def test_a_torn_final_line_is_skipped_not_fatal(tmp_path):
    # kill -9 mid-write leaves a partial line. Losing that one result is
    # correct; refusing to start is not.
    p = tmp_path / "ck.jsonl"
    c = ck(p); c.put(0, "bm25", 3); c.close()
    with p.open("a") as f:
        f.write('{"i": 1, "mode": "bm2')
    c2 = ck(p)
    assert c2.get(0, "bm25") == 3
    assert c2.get(1, "bm25") is run_eval.NOT_DONE
    c2.close()


def test_a_header_only_checkpoint_resumes_from_nothing(tmp_path):
    p = tmp_path / "ck.jsonl"
    ck(p).close()
    c = ck(p)
    assert c.done == {}
    c.close()


def test_a_later_write_wins_for_the_same_pair(tmp_path):
    # Append-only, so a pair rescored after a partial run appears twice. The
    # last one is the one that finished.
    p = tmp_path / "ck.jsonl"
    c = ck(p); c.put(0, "bm25", 5); c.put(0, "bm25", 2); c.close()
    c2 = ck(p)
    assert c2.get(0, "bm25") == 2
    c2.close()


def test_the_header_is_written_once_not_per_resume(tmp_path):
    p = tmp_path / "ck.jsonl"
    ck(p).close()
    ck(p).close()
    ck(p).close()
    lines = p.read_text().splitlines()
    assert len(lines) == 1 and json.loads(lines[0]) == HDR
