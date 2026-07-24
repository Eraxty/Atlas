import nntp

class NNTPClient:
    def __init__(self, host, username, password, port=563):
        self.host = host
        self.port = port
        self.username = username
        self.password = password
        self.server = None

    def connect(self):
        self.server = nntp.NNTPClient(
            host=self.host,
            port=self.port,
            username=self.username,
            password=self.password,
            use_ssl=True
        )

    def disconnect(self):
        if self.server:
            self.server.quit()

    def select_group(self, group_name):
        return self.server.group(group_name)

    def fetch_headers(self, start, end):
        return self.server.xover((start, end))