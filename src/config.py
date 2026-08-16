import json
import os
from pathlib import Path

BASE_DIR = Path(__file__).resolve().parent.parent
config_file = BASE_DIR / "config.json"


def load_config():
    if not os.path.exists(config_file):
        return None

    try:
        with open(config_file, "r") as f:
            return json.load(f)
    
    except (OSError, json.JSONDecodeError):
        return None


def save_config(host, username, password, port, group, index_mode="dynamic"):
    config = {
        "host": host,
        "username": username,
        "password": password,
        "port": port,
        "group": group,
        "index_mode": index_mode
    }

    config_file.parent.mkdir(parents = True, exist_ok = True)

    #temp file + rename soo the indexer never sees a half written config
    tmp = config_file.with_suffix(".json.tmp")
    with open(tmp, "w") as f:
        json.dump(config, f, indent=4)

    os.replace(tmp, config_file)