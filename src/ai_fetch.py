from src.nntp_client import NNTPClient
from src.parser import parse_subject


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


def fetch_releases(config, groups, keywords, max_per_group=500):

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
