from src.nzb import generate_nzb
from src.sab import configure_watched_dir, get_complete_dir, is_running, job_in_sab, start, wait_ready
from src.search import get_articles, get_release
from src.colors import red, green, yellow, reset

def download_release(release_id):
    release = get_release(release_id)
    articles = get_articles(release_id)

    if release is None:
        print(f"{red}Release not found{reset}")
        return False

    if not articles:
        print(f"{red}No articles found{reset}")
        return False

    first_file = articles[0][1]
    complete_dir = get_complete_dir()

    if (complete_dir / first_file).exists():
        print(f"{yellow}already downloaded{reset}")
        return False

    if not is_running():
        if not start():
            print(f"{red}couldnt start sabnzbd{reset}")
            return False

    if not wait_ready():
        print(f"{yellow}sab isnt ready{reset}")
        return False

    watched_dir = configure_watched_dir()

    if (watched_dir / f"{release[1].replace('/', '_')}.nzb").exists():
        print(f"{yellow}already queued{reset}")
        return False

    generate_nzb(
        release_id,
        output_dir=watched_dir,
    )

    status = job_in_sab(release[1])

    if status == "queued":
        print(f"{green}download queued{reset}")
        return True
    elif status:
        print(f"{yellow}already did this one bruh: {status}{reset}")
    else:
        print(f"{red}couldnt confirm it{reset}")
        print(f"{red}download failed{reset}")

    return False