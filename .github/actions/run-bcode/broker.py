"""GitHub OIDC lease exchange. Secrets are data, never shell commands or config values."""
import json
import os
import pathlib
import sys
import time
import urllib.parse
import urllib.request

LIMIT = 65536


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("redirect forbidden")


def broker_url(value):
    parsed = urllib.parse.urlsplit(value)
    if (parsed.scheme != "https" or not parsed.hostname or parsed.username is not None
            or parsed.password is not None or parsed.path not in ("", "/")
            or parsed.query or parsed.fragment or any(c.isspace() for c in value)):
        raise ValueError("invalid broker URL")
    return value.rstrip("/")


def token(value):
    if not isinstance(value, str) or not value or len(value) > 16384 or not value.isascii() or any(ord(c) < 33 or ord(c) > 126 for c in value):
        raise ValueError("invalid token")
    return value


def request_json(url, bearer, method):
    request = urllib.request.Request(url, headers={"Authorization": "Bearer " + token(bearer)}, method=method)
    with urllib.request.build_opener(NoRedirect()).open(request, timeout=30) as response:
        if response.status != 200:
            raise ValueError("request rejected")
        body = response.read(LIMIT + 1)
    if len(body) > LIMIT:
        raise ValueError("response too large")
    return json.loads(body)


def exchange(base, audience, request_url, request_token, fetch=request_json):
    base = broker_url(base)
    if not audience or len(audience) > 2048 or any(ord(c) < 32 for c in audience):
        raise ValueError("invalid audience")
    parsed = urllib.parse.urlsplit(request_url)
    if (parsed.scheme != "https" or not parsed.hostname or not parsed.hostname.endswith(".actions.githubusercontent.com")
            or parsed.username is not None or parsed.password is not None or parsed.fragment):
        raise ValueError("invalid issuer request URL")
    query = urllib.parse.parse_qsl(parsed.query, keep_blank_values=True)
    query = [(key, value) for key, value in query if key != "audience"] + [("audience", audience)]
    url = urllib.parse.urlunsplit(parsed._replace(query=urllib.parse.urlencode(query)))
    identity = fetch(url, request_token, "GET")
    lease = fetch(base + "/v1/leases", token(identity["value"]), "POST")
    value = token(lease["lease"])
    expiry = lease["expires_at"]
    if type(expiry) is not int or not time.time() + 15 < expiry <= time.time() + 300:
        raise ValueError("invalid lease expiry")
    return value, expiry


def config(base):
    base = broker_url(base)
    return ('[model.profiles.github-broker]\n'
            'provider_plugin_id = "bcode.auth-broker-provider"\n'
            'model_id = "broker-scoped"\n'
            '[model.profiles.github-broker.settings]\n'
            'broker_url = ' + json.dumps(base) + '\n')


def main():
    directory = pathlib.Path(sys.argv[1])
    value, expiry = exchange(os.environ["BCODE_ACTION_BROKER_URL"], os.environ["BCODE_ACTION_BROKER_AUDIENCE"],
                             os.environ["ACTIONS_ID_TOKEN_REQUEST_URL"], os.environ["ACTIONS_ID_TOKEN_REQUEST_TOKEN"])
    # Runner-private storage under umask 077; no JWT retained, no secret in config/output/logs.
    for name, contents in [("broker-token", value), ("broker-expiry", str(expiry)),
                           ("broker.toml", config(os.environ["BCODE_ACTION_BROKER_URL"]))]:
        with (directory / name).open("x", encoding="utf-8") as stream:
            stream.write(contents + ("\n" if name != "broker.toml" else ""))
    budget = int(os.environ["BCODE_ACTION_TIMEOUT"])
    if budget > expiry - int(time.time()) - 10:
        raise ValueError("execution budget exceeds lease")


if __name__ == "__main__":
    try:
        main()
    except Exception:
        print("Bcode broker setup failed; check trusted URL, OIDC permissions, scope and lease budget", file=sys.stderr)
        sys.exit(1)
