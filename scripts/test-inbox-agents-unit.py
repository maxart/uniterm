#!/usr/bin/env python3
"""Offline regression tests for the live evaluator; never starts a model."""
import sys
sys.dont_write_bytecode = True
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("evaluation", Path(__file__).with_name("test-inbox-agents.py"))
evaluation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(evaluation)


class ProbeExecuted(Exception):
    pass


class EvaluatorTests(unittest.TestCase):
    def test_worker_records_failure_only_after_native_process_exits(self):
        for provider in evaluation.PROVIDERS:
            with self.subTest(provider=provider), patch("builtins.open"), \
                 patch.object(evaluation.Path, "write_text") as completion, \
                 patch.object(evaluation.subprocess, "Popen") as model:
                def finished():
                    completion.assert_not_called()
                    return 7
                model.return_value.wait.side_effect = finished
                evaluation.worker(provider, "inbox", ["Synthetic task"])
                model.assert_called_once()
                self.assertIn("Synthetic task", model.call_args.args[0])
                self.assertEqual(json.loads(completion.call_args.args[0])["exit_code"], 7)

    def test_usage_does_not_double_count_cache_or_streaming_updates(self):
        codex = evaluation.metrics("codex", [{"type": "turn.completed", "usage": {
            "input_tokens": 100, "cached_input_tokens": 80, "output_tokens": 5}}])
        self.assertEqual((codex["input_tokens"], codex["cached_input_tokens"]), (20, 80))
        for provider, final in [("claude", "result"), ("grok", "end")]:
            events = [{"type": "usage", "usage": {"input_tokens": 999}},
                      {"type": final, "usage": {"input_tokens": 10,
                       "cache_creation_input_tokens": 2, "cache_read_input_tokens": 80},
                       "total_cost_usd": 0.01}]
            usage = evaluation.metrics(provider, events)
            self.assertEqual((usage["input_tokens"], usage["cached_input_tokens"]), (12, 80))
            self.assertEqual(usage["reported_cost_usd"], 0.01)
        usage = evaluation.metrics("opencode", [{"type": "step_finish", "part": {
            "cost": 0, "tokens": {"input": 20, "cache": {"read": 80}, "output": 5}}}])
        self.assertEqual(usage["input_tokens"], 20)
        self.assertIsNone(usage["reported_cost_usd"])

    def test_version_probes_cannot_become_prompts_or_completion_markers(self):
        for provider in evaluation.PROVIDERS:
            for flag in ["--version", "-v", "-V", "--help", "-h"]:
                with self.subTest(provider=provider, flag=flag), \
                     patch.object(evaluation.os, "execv", side_effect=ProbeExecuted) as execute, \
                     patch.object(evaluation.subprocess, "Popen") as model, \
                     patch("builtins.open") as output:
                    with self.assertRaises(ProbeExecuted):
                        evaluation.worker(provider, "inbox", [flag])
                    execute.assert_called_once_with("/opt/" + provider, ["/opt/" + provider, flag])
                    model.assert_not_called()
                    output.assert_not_called()

    def test_no_model_runs_without_explicit_live_opt_in(self):
        with patch.object(evaluation.sys, "argv", ["runner"]), \
             patch.object(evaluation.subprocess, "Popen") as model:
            with self.assertRaises(SystemExit) as error:
                evaluation.main()
            self.assertEqual(error.exception.code, 2)
            model.assert_not_called()


if __name__ == "__main__":
    unittest.main()
