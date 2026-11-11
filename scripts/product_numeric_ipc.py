"""테스트 숫자 IPC는 ambient proxy/redirect 없이 검증된 loopback 주소로만 보낸다."""
import urllib.request


class NoNumericRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def numeric_opener():
    return urllib.request.build_opener(
        urllib.request.ProxyHandler({}), NoNumericRedirect()
    )
