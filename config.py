import json
import os

config_file = "config.json"


def load_config():
    if not os.path.exists(config_file):
        return None

    with open(config_file, "r") as f:
        return json.load(f)


def save_config(host, username, password, port, group):
    config = {
        "host": host,
        "username": username,
        "password": password,
        "port": port,
        "group": group
    }

    with open(config_file, "w") as f:
        json.dump(config, f, indent=4)