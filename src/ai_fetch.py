from src.nntp_client import NNTPClient
from src.parser import parse_subject, group_articles, is_complete
from src.mapper import headers_to_articles
from src.database import save_releases_bulk


def fmt_size(n):
    for u in ["B", "KB", "MB", "GB", "TB"]:
        if n < 1024:
            return f"{n:.1f}{u}"
        n /= 1024
    return f"{n:.1f}PB"


def _get_password(config):
    pwd = config.get("password")
    if pwd:
        return pwd

    try:
        import keyring
        pwd = keyring.get_password("atlas", config.get("username", ""))
    except Exception:
        pass

    return pwd or ""


def fetch_group_candidates(config, limit = 200):
    password = _get_password(config)
    client = NNTPClient(config["host"], config["username"], password, config["port"])

    try:
        client.connect()
    except Exception as e:
        print(f"couldnt connect: {e}")
        return []

    try:
        groups = client.list_groups("alt.binaries*")
    finally:
        client.disconnect()

    return [name for name, count in groups[:limit]]


def fetch_releases(config, groups, keywords, max_per_group = 500):

    password = _get_password(config)

    client = NNTPClient(
        host = config["host"],
        username = config["username"],
        password = password,
        port = config["port"],
    )

    results = []

    try:
        client.connect()
    except Exception as e:
        print(f"couldnt connect: {e}")
        return results

    try:
        for grp in groups:
            try:
                count, first, last, name = client.select_group(grp)
            except Exception as e:
                print(f"couldnt select {grp}: {e}")
                continue

            if last <= first:
                print(f"{grp} is empty")
                continue

            start = max(first, last - max_per_group + 1)
            print(f"fetching {grp} [{start}-{last}]...")

            try:
                headers = list(client.fetch_headers(start, last))
            except Exception as e:
                print(f"fetch failed on {grp}: {e}")
                continue

            for num, hdr in headers:
                subject = hdr.get("subject", "")
                parsed = parse_subject(subject)

                if not parsed:
                    continue

                name_lower = subject.lower()

                if keywords:
                    match = any(kw.lower() in name_lower for kw in keywords)
                    if not match:
                        continue

                results.append({
                    "group": grp,
                    "name": parsed["release_name"],
                    "subject": subject,
                    "size": hdr.get("bytes", 0),
                    "part": parsed["part"],
                    "total": parsed["total_parts"],
                    "poster": hdr.get("from", ""),
                    "date": hdr.get("date", ""),
                })

    finally:
        client.disconnect()

    return results


def fetch_and_store(config, groups, keywords, max_per_group = 500):
    password = _get_password(config)
    client = NNTPClient(config["host"], config["username"], password, config["port"])
    saved = 0

    try:
        client.connect()
    
    except Exception as e:
        print(f"couldnt connect: {e}")
        return 0

    try:
        for grp in groups:
            try:
                count, first, last, name = client.select_group(grp)
            except Exception as e:
                print(f"couldnt select {grp}: {e}")
                continue

            if last <= first:
                print(f"{grp} empty")
                continue

            start = max(first, last - max_per_group + 1)
            print(f"fetching {grp} [{start}-{last}]...")

            try:
                headers = list(client.fetch_headers(start, last))
            except Exception as e:
                print(f"fetch failed {grp}: {e}")
                continue

            articles = headers_to_articles(headers)
            releases = group_articles(articles)
            to_save = []

            for rel in releases.values():
                rel["complete"] = is_complete(rel)
                rel["group"] = grp
                rel["poster"] = rel["articles"][0].author
                rel["date"] = rel["articles"][0].date
                to_save.append(rel)

            if keywords and to_save:
                matched = [r for r in to_save if any(kw.lower() in r["name"].lower() for kw in keywords)]

                if matched:
                    to_save = matched

            if to_save:
                save_releases_bulk(to_save)
                saved += len(to_save)
                print(f"saved {len(to_save)} from {grp}")

    finally:
        client.disconnect()

    return saved
