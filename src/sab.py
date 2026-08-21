from pathlib import Path
import configparser
import json
import re
import subprocess
import sys
import time
import urllib.request

from src.config import load_config as load_atlas_config
from src.colors import red, green, yellow, reset

BASE_DIR = Path(__file__).resolve().parent.parent
SAB_DIR = BASE_DIR / "SABnzbd-5.0.4"
LOG_FILE = BASE_DIR / "sabnzbd.log"

#sab keeps its ini in the home dir
CONFIG_DIR = Path.home() / ".sabnzbd"
CONFIG_FILE = CONFIG_DIR / "sabnzbd.ini"
WATCHED_DIR = CONFIG_DIR / "watched"

process = None


def configure_servers():
    atlas = load_atlas_config()

    if not atlas or not atlas.get("host"):
        return

    try:
        atlas["port"] = int(atlas.get("port", 563))

    except (TypeError, ValueError):
        atlas["port"] = 563

    text = ""
    if CONFIG_FILE.exists():
        text = CONFIG_FILE.read_text(encoding ="utf-8")

    #if the server is already configured, keep its creds in sync instead of duping it
    for section in re.finditer(
        r"^\[\[s(\d+)\]\](.*?)(?=^\[\[s\d+\]\]|\Z)", text, re.MULTILINE | re.DOTALL):
        
        body = section.group(2)

        if not re.search(rf"^host\s*=\s*['\"]?{re.escape(atlas['host'])}['\"]?\s*$", body, re.MULTILINE):
            continue

        creds = dict(re.findall(r"^(port|username|password)\s*=\s*(.*)$", body, re.MULTILINE))

        if (creds.get("port", "").strip() == str(atlas.get("port", 563)) #creds match soo nothing to change
                and creds.get("username", "").strip() == atlas.get("username", "")
                and creds.get("password", "").strip() == atlas.get("password", "")):
            return 

        #creds changed soo swap em in
        current_port = int(atlas.get("port", 563))

        for key, value in (
            ("port", str(current_port)),
            ("username", atlas.get("username", "")),
            ("password", atlas.get("password", "")),
            ("ssl", "1" if current_port == 563 else "0"),):
            
            body = re.sub(rf"^({key}\s*=\s*).*$", lambda m: m.group(1) + value, body, count = 1, flags = re.MULTILINE)

        text = text[:section.start()] + f"[[s{section.group(1)}]]" + body + text[section.end():]

        CONFIG_DIR.mkdir(parents = True, exist_ok = True)
        CONFIG_FILE.write_text(text, encoding = "utf-8")
        return

    sections = re.findall(r"^\[\[s(\d+)\]\]\s*$", text, re.MULTILINE)
    #name it after the highest sN we already got
    index = max([int(n) for n in sections], default=-1) + 1

    try:
        port = int(atlas.get("port", 563))

    except (TypeError, ValueError):
        port = 563

    #563 is the ssl port, 119 is plain
    ssl = "1" if port == 563 else "0"

    block = (
        f"[[s{index}]]\n"
        f"name = s{index}\n"
        f"displayname = {atlas['host']}\n"
        f"host = {atlas['host']}\n"
        f"port = {port}\n"
        f"timeout = 60\n"
        f"username = {atlas.get('username', '')}\n"
        f"password = {atlas.get('password', '')}\n"
        f"connections = 8\n"
        f"ssl = {ssl}\n"
        f"ssl_verify = 1\n"
        f"ssl_ciphers = \"\"\n"
        f"enable = 1\n"
        f"required = 0\n"
        f"optional = 0\n"
        f"pipelining_requests = 2\n"
        f"retention = 0\n"
        f"expire_date = \"\"\n"
    )

    if "\n[servers]" in text:
        text = text.replace("\n[servers]", "\n[servers]\n" + block, 1)
    else:
        text += "\n[servers]\n" + block

    CONFIG_DIR.mkdir(parents = True, exist_ok = True)
    CONFIG_FILE.write_text(text, encoding="utf-8")


def start():
    global process

    if process and process.poll() is None:
        #already up so dont double start
        print(f"{yellow}sab already running{reset}")
        return True

    configure_watched_dir()
    configure_servers()

    try:
        with LOG_FILE.open("a") as log_file:
            process = subprocess.Popen(
                [sys.executable, "-u", "SABnzbd.py"],
                cwd = SAB_DIR,
                stdin = subprocess.DEVNULL,
                stdout = log_file,
                stderr = subprocess.STDOUT,
                #detach from our terminal soo it survives after the app closes
                start_new_session = True,
            )
    
    except OSError as e:
        print(f"{red}couldnt start sabnzbd: {e}{reset}")
        process = None
        return False

    print(f"{green}started sabnzbd{reset}")
    return True


def stop():
    global process

    if process and process.poll() is None:
        try:
            process.terminate()
            process.wait()
    
        except OSError as e:
            print(f"{red}couldnt stop sab: {e}{reset}")
    
        else:
            print(f"{green}Stopped{reset}")
    else:
        print(f"{yellow}SAB isn't running{reset}")


def is_running():
    global process

    if process and process.poll() is None:
        return True

    try:
        with urllib.request.urlopen(get_url(), timeout=2):
            return True

    except OSError:
        return False


def load_config():
    #strict off soo dup keys dont explode, interpolation off soo % stays literal
    config = configparser.ConfigParser(interpolation = None, strict = False)

    if CONFIG_FILE.exists():
        
        try:
            with open(CONFIG_FILE, encoding="utf-8") as f:
                text = f.read()
        
        except OSError:
            text = ""

        #sab writes bare settings above the first section
        lines = text.splitlines()
        first = 0
        
        while first < len(lines) and not lines[first].lstrip().startswith("["):
            first += 1

        bare = [line for line in lines[:first] if line.strip()]
        rest = "\n".join(lines[first:])

        if bare and not re.search(r"^\[DEFAULT\]\s*$", rest, re.MULTILINE):
            rest = "[DEFAULT]\n" + "\n".join(bare) + "\n" + rest

        try:
            config.read_string(rest)
        
        except configparser.Error:
            pass

    if "misc" not in config:
        #guarantee misc exists soo the getters below dont KeyError
        config["misc"] = {}

    return config


def save_config(config):
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)

    with open(CONFIG_FILE, "w") as f:
        config.write(f)


def get_watched_dir():
    config = load_config()

    folder = config["misc"].get("dirscan_dir")

    if folder:
        folder = folder.strip().strip('"').strip()

    if not folder:
        return None

    path = Path(folder)

    if not path.is_absolute():
        #sab saves relative paths soo resolve em against the config dir
        path = CONFIG_DIR / path

    return path


def configure_watched_dir():
    folder = get_watched_dir()

    if folder is None:
        #no watched dir set = use ours and write it in soo sab picks it up
        folder = WATCHED_DIR

        config = load_config()
        config["misc"]["dirscan_dir"] = str(folder)
        save_config(config)

    folder.mkdir(parents=True, exist_ok=True)

    return folder


def get_complete_dir():
    config = load_config()

    folder = config["misc"].get("complete_dir")

    if folder:
        folder = folder.strip().strip('"').strip()

    if not folder:
        return Path.home() / "Downloads/complete"

    path = Path(folder)

    if not path.is_absolute():
        #relative here means against the home dir
        path = Path.home() / path

    return path


def get_api_key():
    return load_config()["misc"].get("api_key", "")


def job_in_sab(name, timeout=10):
    key = get_api_key()
    queue_url = get_url() + f"api?mode=queue&output=json&apikey={key}"
    history_url = get_url() + f"api?mode=history&output=json&apikey={key}&start=0&limit=50"

    deadline = time.time() + timeout

    while time.time() < deadline:
        try:
            with urllib.request.urlopen(queue_url, timeout=2) as r:
                slots = json.load(r).get("queue", {}).get("slots", [])

            if any(s.get("filename", "") in (name, name + ".nzb") for s in slots):
                #still in the queue
                return "queued"

        except (OSError, ValueError):
            pass

        try:
            with urllib.request.urlopen(history_url, timeout=2) as r:
                slots = json.load(r).get("history", {}).get("slots", [])

            for s in slots:
                if s.get("name", "") == name:
                    return s.get("status", "done").lower()

        except (OSError, ValueError):
            pass

        time.sleep(1)

    return None


def get_url():
    config = load_config()

    host = config["misc"].get("host", "127.0.0.1")
    port = config["misc"].get("port", "8080")

    return f"http://{host}:{port}/"


def wait_ready(timeout=60):
    global process

    deadline = time.time() + timeout

    while time.time() < deadline:

        try:
            with urllib.request.urlopen(get_url(), timeout=2):
                return True

        except OSError:
            pass

        if process and process.poll() is not None:
            return False

        time.sleep(1)

    return False