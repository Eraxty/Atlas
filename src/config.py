import json
import os
import keyring
from pathlib import Path

BASE_DIR = Path(__file__).resolve().parent.parent
config_file = BASE_DIR / "config.json"
SERVICE = "atlas"


def load_config():
    if not os.path.exists(config_file):
        return None

    try:
        with open(config_file, "r") as f:
            config = json.load(f)

    except (OSError, json.JSONDecodeError):
        return None

    try:
        password = keyring.get_password(SERVICE, config.get("username", ""))
    except keyring.errors.KeyringError:
        password = None

    if password:
        config["password"] = password

    return config


def save_config(host, username, password, port, group, index_mode="dynamic"):
    try:
        keyring.set_password(SERVICE, username, password)
        store_password = False
    except keyring.errors.KeyringError:
        store_password = True

    config = {
        "host": host,
        "username": username,
        "port": port,
        "group": group,
        "index_mode": index_mode
    }

    if store_password:
        config["password"] = password

    config_file.parent.mkdir(parents = True, exist_ok = True)

    #temp file + rename soo the indexer never sees a half written config
    tmp = config_file.with_suffix(".json.tmp")
    with open(tmp, "w") as f:
        json.dump(config, f, indent=4)

    os.replace(tmp, config_file)