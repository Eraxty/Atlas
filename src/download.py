from src.nzb import generate_nzb
from src.sab import configure_watched_dir, get_complete_dir, is_running, job_in_sab, start, wait_ready
from src.search import get_articles, get_release

def download_release(release_id):
    release = get_release(release_id)
    articles = get_articles(release_id)

    if release is None:
        print("Release not found")
        return False

    if not articles:
        print("No articles found")
        return False

    if (get_complete_dir() / articles[0][1]).exists():
        print("Already downloaded.")
        return False

    if not is_running():
        if not start():
            print("Couldn't start SABnzbd.")
            return False

    if not wait_ready():
        print("SABnzbd isn't ready.")
        return False

    watched_dir = configure_watched_dir()

    if (watched_dir / f"{release[1].replace('/', '_')}.nzb").exists():
        print("Already queued.")
        return False

    generate_nzb(
        release_id,
        output_dir=watched_dir,
    )

    status = job_in_sab(release[1])

    if status == "queued":
        print("SAB accepted the download.")
    elif status:
        print(f"SAB already processed this download: {status}.")
    else:
        print("Couldn't confirm SAB received the download.")

    print("Download queued.")
    return True