import re
import sys
import tempfile

from src.paths import app_dir

BASE_PAR2_RE = re.compile(r'\.par2(?:["\s]|$)', re.IGNORECASE)
VOL_PAR2_RE = re.compile(r'\.vol\d+[+-]\d+\.par2', re.IGNORECASE)


def is_base_par2(subject):
    return bool(BASE_PAR2_RE.search(subject or '')) and not VOL_PAR2_RE.search(subject or '')


def display_name(data):
    sab_dir = app_dir() / 'SABnzbd-5.0.4'

    if str(sab_dir) not in sys.path:
        sys.path.insert(0, str(sab_dir))

    from sabnzbd.par2file import parse_par2_file

    with tempfile.NamedTemporaryFile(suffix='.par2') as parfile:
        parfile.write(data)
        parfile.flush()
        _, files = parse_par2_file(parfile.name, {})

    if not files:
        return None

    return max(files.values(), key = lambda file: file.filesize).filename

