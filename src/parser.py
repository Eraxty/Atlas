import re


def parse_subject(subject):
    pattern = r'"(.+?)"\s+yEnc\s+\((\d+)/(\d+)\)'
    match = re.search(pattern, subject)
    
    if not match:
        return None
    
    filename = match.group(1)
    release_name = re.sub(
        r"\.part\d+\.rar$",
        "",
        filename,
        flags=re.IGNORECASE
    )

    return {
        "filename": filename,
        "release_name": release_name,
        "part": int(match.group(2)),
        "total_parts": int(match.group(3))
    }


def group_articles(articles):
    releases = {}
    
    for article in articles:
        parsed = parse_subject(article.subject)
    
        if not parsed:
    
            continue
        name = parsed["release_name"]
    
        if name not in releases:
            releases[name] = {
                "name": name,
                "articles": [],
                "size": 0
            }
    
        article.filename = parsed["filename"]
        article.release_name = parsed["release_name"]
        article.part = parsed["part"]
        article.total_parts = parsed["total_parts"]
        releases[name]["articles"].append(article)
        releases[name]["size"] += article.bytes
    return releases


def is_complete(release):
    articles = release["articles"]
    
    if not articles:
        return False
    
    expected = articles[0].total_parts
    parts = {
        article.part
        for article in articles
    }

    expected_parts = set(range(1, expected + 1))
    return parts == expected_parts
