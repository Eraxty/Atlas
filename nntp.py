import nntplib

class NNTPClient:
    def __init__(self, host, username, password, port=563):
        self.host = host
        self.port = port
        self.username = username
        self.password = password
        self.server = None

    def connect(self):
        self.server = nntplib.NNTP_SSL(
            self.host,
            user=self.username,
            password=self.password,
            port=self.port
        )

    def disconnect(self):
        if self.server:
            self.server.quit()
    
    def select_group(self,group_name):
        return self.server.group(group_name)

    def fetch_headers(self,start,end):
        _, headers = self.server.over((start,end))
        return headers
