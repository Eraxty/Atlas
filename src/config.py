import json
import os
from pathlib import Path

BASE_DIR = Path(__file__).resolve().parent.parent
config_file = BASE_DIR / "config.json"


def load_config():
    if not os.path.exists(config_file):
        return None

    with open(config_file, "r") as f:
        return json.load(f)


def save_config(host, username, password, port, group, index_mode="dynamic"):
    config = {
        "host": host,
        "username": username,
        "password": password,
        "port": port,
        "group": group,
        "index_mode": index_mode
    }

    with open(config_file, "w") as f:
        json.dump(config, f, indent=4)