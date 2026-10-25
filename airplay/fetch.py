import concurrent.futures
import hashlib
import json
import subprocess

PROJECTS = {
    "shairport-sync": ("mikebrady/shairport-sync", "b991d91350e537c1f50dac98d9a3c836eceddcfc"),
    "nqptp": ("mikebrady/nqptp", "ab8fb67483411640fd7f1e193889482f2421c37c"),
}


def prepare(root, recipe):
    downloads = root / "downloads"
    downloads.mkdir(exist_ok=True)
    sources = json.loads((recipe / "sources.json").read_text())

    def fetch(item):
        archive = downloads / item["url"].rsplit("/", 1)[1]
        if not archive.exists():
            temporary = archive.with_name(archive.name + ".tmp")
            subprocess.run(["curl", "--fail", "--location", "--retry", "2", "--silent", "--show-error", "--connect-timeout", "20", "--max-time", "240", "-o", str(temporary), item["url"]], check=True)
            temporary.rename(archive)
        if item.get("sha256") and hashlib.sha256(archive.read_bytes()).hexdigest() != item["sha256"]:
            raise RuntimeError("Source checksum mismatch: " + item["name"])
        destination = root / item["name"]
        if not destination.exists():
            destination.mkdir()
            subprocess.run(["tar", "-xf", str(archive), "--strip-components=1", "-C", str(destination)], check=True)

    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        list(pool.map(fetch, sources))
    for name, (repo, revision) in PROJECTS.items():
        destination = root / name
        if (destination / ".git").exists():
            actual = subprocess.check_output(["git", "-C", str(destination), "rev-parse", "HEAD"], text=True).strip()
            if actual != revision:
                raise RuntimeError("Unexpected source revision: " + name)
        else:
            fetch({"name": name, "url": f"https://codeload.github.com/{repo}/tar.gz/{revision}"})
