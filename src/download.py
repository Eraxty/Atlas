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

    first_file = articles[0][1]
    complete_dir = get_complete_dir()

    if (complete_dir / first_file).exists():
        print("already downloaded")
        return False

    if not is_running():
        if not start():
            print("couldnt start sabnzbd")
            return False

    if not wait_ready():
        print("sab isnt ready")
        return False

    watched_dir = configure_watched_dir()

    if (watched_dir / f"{release[1].replace('/', '_')}.nzb").exists():
        print("already queued")
        return False

    generate_nzb(
        release_id,
        output_dir=watched_dir,
    )

    status = job_in_sab(release[1])

    if status == "queued":
        print("done")
    elif status:
        print(f"already did this one bruh: {status}")
    else:
        print("couldnt confirm it")

    print("download queued")
    return True