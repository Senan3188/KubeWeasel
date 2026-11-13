"""Owner-session headers for opt-in live checks. Never prints credentials."""
import json
import os
import urllib.request


def auth_headers(base):
    cookie = os.environ.get('KUBEWEASEL_SESSION_COOKIE')
    if not cookie:
        raise RuntimeError('Sign in first, then set KUBEWEASEL_SESSION_COOKIE to the session cookie for this test process.')
    request = urllib.request.Request(base + '/api/auth/status', headers={'cookie': cookie})
    with urllib.request.urlopen(request, timeout=15) as response:
        status = json.load(response)
    if not status.get('authenticated'):
        raise RuntimeError('The owner session has expired; sign in again.')
    return {'content-type': 'application/json', 'x-kubeweasel-request': 'dashboard',
            'cookie': cookie, 'x-kubeweasel-csrf': status['csrf']}
