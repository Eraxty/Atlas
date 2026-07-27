import nntp


class NNTPClient:
    def __init__(self, host, username, password, port=563, use_ssl=None):
        self.host = host
        self.port = port
        self.username = username
        self.password = password
        self.use_ssl = self._detect_use_ssl(host, port) if use_ssl is None else use_ssl
        self.server = None

    @staticmethod
    def _detect_use_ssl(host, port):
        try:
            if int(port) == 563:
                return True
        except (TypeError, ValueError):
            pass

        normalized_host = str(host or "").strip().strip("[]").rstrip(".").lower()
        if normalized_host in {"localhost", "127.0.0.1", "::1"}:
            return False
        if normalized_host.endswith(".local"):
            return False

        return True

    def connect(self):
        self.server = nntp.NNTPClient(
            host=self.host,
            port=self.port,
            username=self.username,
            password=self.password,
            use_ssl=self.use_ssl
        )

    def disconnect(self):
        if self.server:
            self.server.quit()

    def select_group(self, group_name):
        return self.server.group(group_name)

    def fetch_headers(self, start, end):
        return self.server.xover((start, end))
