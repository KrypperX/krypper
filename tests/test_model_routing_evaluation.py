import json
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import model_routing_evaluation as evaluation  # noqa: E402


class ModelRoutingEvaluationTests(unittest.TestCase):
    def test_parses_only_the_selected_route_line(self):
        self.assertEqual(
            evaluation.parse_selected_route(
                "provider warning\nkrypper: selected route cloudflare/@cf/model (lowest_latency)\n"
            ),
            {
                "provider": "cloudflare",
                "model": "@cf/model",
                "reason": "lowest_latency",
            },
        )
        self.assertIsNone(evaluation.parse_selected_route("provider warning\n"))

    def test_cost_estimate_uses_usage_and_catalog_rates_in_micro_usd(self):
        events = [{"prompt_tokens": 1000, "completion_tokens": 500}]
        pricing = {
            "input_usd_per_million_tokens": 2,
            "output_usd_per_million_tokens": 8,
        }
        self.assertEqual(evaluation.estimate_cost_micro_usd(events, pricing), 6000)

    def test_cost_estimate_stays_unknown_when_usage_or_rates_are_missing(self):
        self.assertIsNone(
            evaluation.estimate_cost_micro_usd(
                [{"prompt_tokens": 2, "completion_tokens": None}],
                {"input_usd_per_million_tokens": 1, "output_usd_per_million_tokens": 1},
            )
        )
        self.assertIsNone(
            evaluation.estimate_cost_micro_usd(
                [{"prompt_tokens": 2, "completion_tokens": 3}],
                {"input_usd_per_million_tokens": 1},
            )
        )

    def test_router_estimate_uses_catalog_limits_separately_from_observed_usage(self):
        self.assertEqual(
            evaluation.conservative_request_cost_micro_usd(
                {"context_window": 128_000, "max_output_tokens": 16_000},
                {
                    "input_usd_per_million_tokens": 2,
                    "output_usd_per_million_tokens": 8,
                },
            ),
            384_000,
        )

    def test_config_is_per_arm_and_contains_no_credentials(self):
        value = evaluation.config_contents("cloudflare", "@cf/model", "automatic")
        self.assertIn('provider = "cloudflare"', value)
        self.assertIn('model = "@cf/model"', value)
        self.assertIn('routing_mode = "automatic"', value)
        self.assertNotIn("API_KEY", value)
        self.assertNotIn("token", value.lower())

    def test_isolated_settings_copy_only_public_evidence_and_catalog_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "source"
            destination = Path(temporary) / "isolated"
            source.mkdir()
            for name in (
                "model-evidence.json",
                "model-evidence.key",
                "model-catalog-cache.json",
                "credentials.json",
                "provider-profiles.json",
            ):
                (source / name).write_text(name, encoding="utf-8")

            copied = evaluation.copy_public_settings(source, destination)

            self.assertEqual(
                copied,
                ["model-catalog-cache.json", "model-evidence.json", "model-evidence.key"],
            )
            self.assertFalse((destination / "credentials.json").exists())
            self.assertFalse((destination / "provider-profiles.json").exists())

    def test_offline_environment_removes_provider_secrets(self):
        environment = evaluation.environment_without_provider_credentials(
            {
                "OPENAI_API_KEY": "secret",
                "CLOUDFLARE_API_TOKEN": "secret",
                "KRYPPER_CONFIG": "/user/config.toml",
                "PATH": "/bin",
            }
        )
        self.assertNotIn("OPENAI_API_KEY", environment)
        self.assertNotIn("CLOUDFLARE_API_TOKEN", environment)
        self.assertNotIn("KRYPPER_CONFIG", environment)
        self.assertEqual(environment["PATH"], "/bin")

    def test_live_environment_keeps_only_selected_provider_credentials(self):
        environment = evaluation.environment_for_provider(
            "cloudflare-ai-gateway",
            {
                "CLOUDFLARE_ACCOUNT_ID": "account",
                "CLOUDFLARE_API_TOKEN": "token",
                "CLOUDFLARE_AI_GATEWAY_ID": "gateway",
                "OPENAI_API_KEY": "other-provider-secret",
                "PATH": "/bin",
            },
        )
        self.assertEqual(environment["CLOUDFLARE_ACCOUNT_ID"], "account")
        self.assertEqual(environment["CLOUDFLARE_API_TOKEN"], "token")
        self.assertEqual(environment["CLOUDFLARE_AI_GATEWAY_ID"], "gateway")
        self.assertNotIn("OPENAI_API_KEY", environment)
        self.assertEqual(environment["PATH"], "/bin")

    def test_live_preflight_reports_missing_variable_names_without_values(self):
        with self.assertRaisesRegex(RuntimeError, "CLOUDFLARE_API_TOKEN") as error:
            evaluation.validate_provider_credentials(
                "cloudflare",
                {"CLOUDFLARE_ACCOUNT_ID": "account", "CLOUDFLARE_API_TOKEN": ""},
            )
        self.assertNotIn("account", str(error.exception))

    def test_existing_fixture_suite_covers_required_task_categories(self):
        suite = json.loads(evaluation.SUITE.read_text(encoding="utf-8"))
        categories = {
            category
            for task in suite["tasks"]
            for category in task.get("categories", [])
        }
        self.assertTrue({"coding", "reasoning", "tool_use"}.issubset(categories))

    def test_report_summarizes_each_fixture_category_per_arm(self):
        summaries = evaluation.category_summaries(
            [
                {
                    "arm": "manual",
                    "live": True,
                    "categories": ["coding", "tool_use"],
                    "success": True,
                    "end_to_end_latency_ms": 50,
                    "safety_incidents": 0,
                    "duplicate_effects": 0,
                }
            ]
        )
        self.assertEqual(summaries["coding"]["manual"]["task_successes"], 1)
        self.assertEqual(summaries["reasoning"]["manual"]["runs"], 0)
        self.assertEqual(summaries["tool_use"]["automatic"]["runs"], 0)

    def test_readiness_requires_complete_independent_route_and_usage_evidence(self):
        route = {"provider": "cloudflare", "model": "@cf/model", "reason": "manual_override"}
        routes = {"manual": route, "automatic": route}
        runs = [
            {
                "live": True,
                "model_decisions": 1,
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "model_decision_latency_ms": 30,
                "independent_cargo_test_exit_code": 0,
                "selected_route": route,
                "route_matches_preflight": True,
            }
            for _ in range(4 * 3 * 2)
        ]

        readiness = evaluation.sample_readiness(runs, 24, 3, 4, routes)
        self.assertTrue(readiness["ready_for_issue_acceptance"])
        self.assertTrue(readiness["matched_model_comparison_ready"])

        runs[0]["prompt_tokens"] = None
        self.assertFalse(
            evaluation.sample_readiness(runs, 24, 3, 4, routes)["ready_for_issue_acceptance"]
        )

        routes["automatic"] = {**route, "model": "different-model"}
        runs[0]["prompt_tokens"] = 10
        readiness = evaluation.sample_readiness(runs, 24, 3, 4, routes)
        self.assertTrue(readiness["ready_for_issue_acceptance"])
        self.assertFalse(readiness["matched_model_comparison_ready"])


if __name__ == "__main__":
    unittest.main()
