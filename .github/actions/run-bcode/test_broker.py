"""Offline behavioral contract tests; no GitHub or broker network calls."""
import importlib.util
import pathlib
import time
import unittest

spec = importlib.util.spec_from_file_location("broker", pathlib.Path(__file__).with_name("broker.py"))
broker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(broker)


class BrokerTests(unittest.TestCase):
    def test_exchange_treats_jwt_as_data_and_audience_as_query(self):
        calls = []

        def fetch(url, bearer, method):
            calls.append((url, bearer, method))
            if method == "GET":
                return {"value": "issuer.jwt"}
            return {"lease": "opaque-lease", "expires_at": int(time.time()) + 100}

        value, expiry = broker.exchange("https://broker.example", "urn:audience?x=1&y=2",
                                       "https://pipelines.actions.githubusercontent.com/id?api-version=2&audience=old",
                                       "runner-request-token", fetch)
        self.assertEqual(value, "opaque-lease")
        self.assertGreater(expiry, time.time())
        self.assertEqual(len(calls), 2)
        query = broker.urllib.parse.parse_qs(broker.urllib.parse.urlsplit(calls[0][0]).query)
        self.assertEqual(query["audience"], ["urn:audience?x=1&y=2"])
        self.assertEqual(calls[1], ("https://broker.example/v1/leases", "issuer.jwt", "POST"))

    def test_untrusted_urls_fail_before_any_network(self):
        def forbidden(*args):
            self.fail("network attempted")

        for base in ["http://broker.example", "https://u:p@broker.example", "https://broker.example/path", "https://broker.example/?x=y"]:
            with self.assertRaises(ValueError):
                broker.exchange(base, "aud", "https://pipelines.actions.githubusercontent.com/id", "token", forbidden)
        with self.assertRaises(ValueError):
            broker.exchange("https://broker.example", "aud", "https://evil.example/id", "token", forbidden)

    def test_invalid_tokens_and_expiry_fail_closed(self):
        for value in ["", "line\nbreak", "space token", "x" * 16385]:
            with self.assertRaises(ValueError):
                broker.token(value)
        for expiry in [1, int(time.time()) + 1000, True, "123"]:
            def fetch(url, bearer, method):
                return {"value": "jwt"} if method == "GET" else {"lease": "lease", "expires_at": expiry}
            with self.assertRaises(ValueError):
                broker.exchange("https://broker.example", "aud", "https://pipelines.actions.githubusercontent.com/id", "token", fetch)

    def test_config_is_exclusive_and_has_no_credentials(self):
        config = broker.config("https://broker.example")
        self.assertIn('provider_plugin_id = "bcode.auth-broker-provider"', config)
        self.assertIn('model_id = "broker-scoped"', config)
        self.assertNotIn("auth_profile", config)
        self.assertNotIn("token", config)
        self.assertNotIn("openai", config)

    def test_redirect_is_rejected(self):
        with self.assertRaises(ValueError):
            broker.NoRedirect().redirect_request(None, None, 302, "redirect", {}, "https://elsewhere.example")


if __name__ == "__main__":
    unittest.main()
