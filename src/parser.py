import re

FILE_COUNT_RE = re.compile(r'^\[(\d+)\s*/\s*(\d+)\]')

def parse_subject(subject):

    patterns = [
        r'"(.+?)"\s+yEnc\s+\((\d+)\s*/\s*(\d+)\)',
        r'\[\d+\s*/\s*\d+\]\s+"(.+?)"\s+yEnc\s+\((\d+)\s*/\s*(\d+)\)',
        r'\[\d+\s*/\s*\d+\]\s+-\s+"(.+?)"\s+-\s+[\d.]+\s+\w+\s+yEnc\s+\((\d+)\s*/\s*(\d+)\)',
        r'yEnc\s+"(.+?)"\s+[\d.]+\s+\w+\s+\((\d+)\s*/\s*(\d+)\)',
        r'"(.+?)"\s+\((\d+)\s*/\s*(\d+)\)',
        r'([^\s"]+\.[^\s"]+)\s+yEnc\s+\((\d+)\s*/\s*(\d+)\)',
        r'([^\s"]+\.[^\s"]+)\s+\((\d+)\s*/\s*(\d+)\)',
        r'\[\d+\s*/\s*\d+\]\s+([^\s"]+\.[^\s"]+)\s+yEnc',
    ]

    match = None

    for pattern in patterns:
        match = re.search(pattern, subject, re.IGNORECASE)

        if match:
            break

    if not match:
        return None

    filename = match.group(1)

    release_name = filename

    while True:
        new_name = re.sub(
            r"\.(part\d+|r\d+|vol\d+\+\d+|par2|nfo|sfv|rar|\d{3})$",
            "",
            release_name,
            flags=re.IGNORECASE,
        )

        if new_name == release_name:
            break

        release_name = new_name

    if len(match.groups()) >= 3:
        part = int(match.group(2))
        total_parts = int(match.group(3))
    else:
        part = 1
        total_parts = 1

    file_index = None
    file_total = None
    
    count_match = FILE_COUNT_RE.match(subject.strip())

    if count_match:
        file_index = int(count_match.group(1))
        file_total = int(count_match.group(2))

    return {
        "filename": filename,
        "release_name": release_name,
        "part": part,
        "total_parts": total_parts,
        "file_index": file_index,
        "file_total": file_total,
    }


def group_articles(articles):
    releases = {}
    dropped = 0

    for article in articles:
        parsed = parse_subject(article.subject)

        if not parsed:
            dropped += 1
            continue

        name = parsed["release_name"]

        if name not in releases:
            releases[name] = {
                "name": name,
                "articles": [],
                "size": 0,
            }

        article.filename = parsed["filename"]
        article.release_name = parsed["release_name"]
        article.part = parsed["part"]
        article.total_parts = parsed["total_parts"]
        article.file_index = parsed["file_index"]
        article.file_total = parsed["file_total"]

        releases[name]["articles"].append(article)
        releases[name]["size"] += article.bytes

    if dropped:
        print(f"Dropped {dropped}/{len(articles)} unparsable")

    return releases


def is_complete(release):
    articles = release["articles"]

    if not articles:
        return False

    by_filename = {}

    for a in articles:
        by_filename.setdefault(a.filename, []).append(a)

    for files_articles in by_filename.values():
        expected = max(a.total_parts for a in files_articles)    
        parts = {a.part for a in files_articles}

        if parts != set(range(1, expected + 1)):
            return False

        file_totals = {a.file_total for a in articles if a.file_total is not None}

        if len(file_totals) == 1:
            expected_files = file_totals.pop()

            if len(by_filename) != expected_files:
                return False

        return True