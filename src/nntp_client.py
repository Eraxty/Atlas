import nntp


class NNTPClient:
    def __init__(self, host, username, password, port=563, use_ssl=None, timeout=30):
        self.host = host
        self.port = port
        self.username = username
        self.password = password
        self.timeout = timeout
        #auto detect unless the caller says otherwise
        self.use_ssl = self._detect_use_ssl(host, port) if use_ssl is None else use_ssl
        self.server = None

    #guess if the server wants ssl before we connect
    @staticmethod
    def _detect_use_ssl(host, port):
        try:
            if int(port) == 563:
                #563 is the ssl port, 119 is the plain one
                return True
        except (TypeError, ValueError):
            pass

        normalized_host = str(host or "").strip().strip("[]").rstrip(".").lower()
        if normalized_host in {"localhost", "127.0.0.1", "::1"}:
            #local servers are plain usually
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
            use_ssl=self.use_ssl,
            timeout=self.timeout
        )

    def update_credentials(self, host, username, password, port):
        self.host = host
        self.port = port
        self.username = username
        self.password = password
        self.use_ssl = self._detect_use_ssl(host, port)

    def disconnect(self):
        if not self.server:
            return

        try:
            self.server.quit()
        except (OSError, nntp.NNTPError):
            pass
        finally:
            self.server = None

    def select_group(self, group_name):
        code, message = self.server.command("GROUP", group_name)
        if code != 211:
            raise nntp.NNTPReplyError(code, message)

        parts = message.split(None, 4)

        def to_int(value):
            try:
                return int(value)
            except (TypeError, ValueError):
                #numbers aint always there soo dont crash on em
                return 0

        count = to_int(parts[0]) if len(parts) > 0 else 0
        first = to_int(parts[1]) if len(parts) > 1 else 0
        last = to_int(parts[2]) if len(parts) > 2 else 0
        name = parts[3] if len(parts) > 3 else group_name

        return count, first, last, name

    def fetch_headers(self, start, end):
        #xover fetches the whole header range in one shot
        return self.server.xover((start, end))

    def list_groups(self):
        return self.server.list()