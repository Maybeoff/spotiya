import concurrent.futures
import hashlib
import json
import shutil
import subprocess

PROJECTS = {
    "shairport-sync": ("mikebrady/shairport-sync", "b991d91350e537c1f50dac98d9a3c836eceddcfc", "4cd79d94bab1e0eec9669848fc14bfcef677561df051279e59a3905b361ee541"),
    "nqptp": ("mikebrady/nqptp", "ab8fb67483411640fd7f1e193889482f2421c37c", "3e90e2ea1f52f5c1559a5a46b2cdadb46ac6532a08a34f8cd7e77aa39f6a90c7"),
}
# Records which archive a source tree was extracted from, so a version bump replaces it.
MARKER = ".spotiya-source"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def prepare(root, recipe):
    downloads = root / "downloads"
    downloads.mkdir(exist_ok=True)
    sources = json.loads((recipe / "sources.json").read_text())

    def fetch(item):
        archive = downloads / item["url"].rsplit("/", 1)[1]
        if archive.exists() and digest(archive) != item["sha256"]:
            archive.unlink()
        if not archive.exists():
            temporary = archive.with_name(archive.name + ".tmp")
            subprocess.run(["curl", "--fail", "--location", "--retry", "2", "--silent", "--show-error", "--connect-timeout", "20", "--max-time", "240", "-o", str(temporary), item["url"]], check=True)
            if digest(temporary) != item["sha256"]:
                temporary.unlink()
                raise RuntimeError("Source checksum mismatch: " + item["name"])
            temporary.rename(archive)
        destination = root / item["name"]
        marker = destination / MARKER
        if marker.exists() and marker.read_text() == item["sha256"]:
            return
        # Extract beside the old tree first so an interrupted run never leaves a partial source.
        staging = root / (item["name"] + ".extract")
        shutil.rmtree(staging, ignore_errors=True)
        staging.mkdir()
        subprocess.run(["tar", "-xf", str(archive), "--strip-components=1", "-C", str(staging)], check=True)
        (staging / MARKER).write_text(item["sha256"])
        # Build outputs live inside the source tree, so a fresh tree must not look built.
        (root / "logs" / f"{item['name']}.done").unlink(missing_ok=True)
        shutil.rmtree(destination, ignore_errors=True)
        staging.rename(destination)

    for name, (repo, revision, checksum) in PROJECTS.items():
        destination = root / name
        if (destination / ".git").exists():
            actual = subprocess.check_output(["git", "-C", str(destination), "rev-parse", "HEAD"], text=True).strip()
            if actual != revision:
                raise RuntimeError("Unexpected source revision: " + name)
        else:
            sources.append({"name": name, "url": f"https://codeload.github.com/{repo}/tar.gz/{revision}", "sha256": checksum})
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        list(pool.map(fetch, sources))
