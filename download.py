from nzb import generate_nzb
from sab import configure_watched_dir, is_running, start, wait_ready

def download_release(release_id):
    if not is_running():
        if not start():
            print("Couldn't start SABnzbd.")
            return

    if not wait_ready():
        print("SABnzbd isn't ready.")
        return

    watched_dir = configure_watched_dir()

    generate_nzb(
        release_id,
        output_dir=watched_dir,
    )

    print("Download queued.")