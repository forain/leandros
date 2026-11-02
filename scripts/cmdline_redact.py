"""Redact secrets (wifi.psk=, iwlwifi.psk=, *password*=) from kernel command lines.

Anything that may carry /proc/cmdline text to a terminal, log or exception
message goes through redact() first.
"""
import re

_SECRET = re.compile(r"([A-Za-z0-9_.]*(?:psk|password)[A-Za-z0-9_.]*=)\S*", re.I)


def redact(text):
    return _SECRET.sub(r"\1<redacted>", text)
